//! Clipboard support: the system clipboard through `wl-copy`/`xclip`/`xsel`, with the OSC 52
//! terminal escape sequence as fallback.

use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Some terminals reject longer OSC 52 payloads.
pub const MAX_CLIPBOARD_BYTES: usize = 100_000;

/// A forking clipboard tool returns as soon as it owns the selection.
const TOOL_TIMEOUT: Duration = Duration::from_secs(2);

/// Copies to the system clipboard with the first tool that works and returns its name.
pub fn copy_native(text: &str) -> Result<&'static str, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    let mut tools: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if wayland {
        tools.push(("wl-copy", &[]));
    }
    if x11 {
        tools.push(("xclip", &["-selection", "clipboard"]));
        tools.push(("xsel", &["--clipboard", "--input"]));
    }
    if tools.is_empty() {
        return Err("no display (WAYLAND_DISPLAY/DISPLAY) for a clipboard tool".to_owned());
    }
    let mut failures = Vec::new();
    for (program, arguments) in tools {
        match run_tool(program, arguments, text) {
            Ok(()) => return Ok(program),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    Err(failures.join("; "))
}

/// Larger clipboard contents are refused; the text goes into a form field.
pub const MAX_PASTE_BYTES: usize = 4 * 1024 * 1024;

/// Reads the system clipboard with the first tool that works.
pub fn paste_native() -> Result<String, String> {
    let mut tools: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if cfg!(target_os = "macos") {
        tools.push(("pbpaste", &[]));
    } else if cfg!(windows) {
        tools.push((
            "powershell",
            &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
        ));
    } else {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        if wayland {
            tools.push(("wl-paste", &["--no-newline"]));
        }
        if x11 {
            tools.push(("xclip", &["-o", "-selection", "clipboard"]));
            tools.push(("xsel", &["--clipboard", "--output"]));
        }
        if tools.is_empty() {
            return Err("no display (WAYLAND_DISPLAY/DISPLAY) for a clipboard tool".to_owned());
        }
    }
    let mut failures = Vec::new();
    for (program, arguments) in tools {
        match read_tool(program, arguments) {
            Ok(text) => return Ok(text),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    Err(failures.join("; "))
}

fn read_tool(program: &str, arguments: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().ok_or_else(|| "no stdout".to_owned())?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let result = (&mut stdout)
            .take(MAX_PASTE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let outcome = receiver.recv_timeout(TOOL_TIMEOUT);
    let _ = child.kill();
    let status = child.wait();
    match outcome {
        Ok(Ok(bytes)) => {
            if bytes.len() > MAX_PASTE_BYTES {
                return Err(format!("more than {MAX_PASTE_BYTES} bytes"));
            }
            if status.is_ok_and(|status| !status.success() && bytes.is_empty()) {
                return Err("the clipboard is empty or holds no text".to_owned());
            }
            String::from_utf8(bytes)
                .map_err(|_| "the clipboard does not hold UTF-8 text".to_owned())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("timed out".to_owned()),
    }
}

fn run_tool(program: &str, arguments: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| "no stdin".to_owned())
        .and_then(|mut stdin| {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| error.to_string())
        });
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".to_owned());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(input: &[u8]) -> String {
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (value >> (18 - 6 * position)) & 0x3f;
                output.push(char::from(ALPHABET[index as usize]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

pub fn osc52_sequence(text: &str) -> Result<String, String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(format!(
            "{} bytes exceed the {MAX_CLIPBOARD_BYTES}-byte clipboard limit",
            text.len()
        ));
    }
    Ok(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn oversized_text_is_rejected() {
        assert!(osc52_sequence(&"x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_err());
        assert_eq!(osc52_sequence("foo").unwrap(), "\x1b]52;c;Zm9v\x07");
    }
}
