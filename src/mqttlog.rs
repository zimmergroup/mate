//! `.mqttlog` files: one line per received message, one file per broker, day and size block.
//!
//! `2026-10-23 10:53:51,998 | T0   I: <topic> [{"key":"value"}] <payload>`
//!
//! The time is UTC. The layout can be changed in `config.toml` (`[log_format]`, see `LogFormat`).
//!
//! The brackets hold the MQTT 5 user properties as a JSON array with one object per property
//! (`[]` without any). The payload stays on the line as it is, except where that would be
//! ambiguous or break the line: a payload that is not UTF-8 is `0x` and hex digits, and one with
//! control characters (line breaks included) or starting with `0x` or `\:` is `\:` followed by the
//! text with `\\`, `\n`, `\r` and `\u{..}` escapes. `parse_line` reads every form back exactly.
//!
//! Files are named `<broker-id>_<YYYYMMDD>.<n>.mqttlog`; `n` counts up from 0 within a day
//! whenever a file would grow beyond the size limit.

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDateTime, Utc, format::StrftimeItems};
use regex::Regex;

use crate::{config::LogFormatConfig, topics::MessageProperties};

pub const FILE_EXTENSION: &str = "mqttlog";
pub const DEFAULT_MAX_MEGABYTES: u32 = 20;
pub const MAX_MEGABYTES: u32 = 1024;

/// `<broker-id>_<YYYYMMDD>.<n>.mqttlog`
pub fn file_name(broker_id: &str, day: &str, index: u32) -> String {
    format!("{broker_id}_{day}.{index}.{FILE_EXTENSION}")
}

/// The block numbers that already exist for this broker and day, ascending.
fn existing_blocks(directory: &Path, broker_id: &str, day: &str) -> Vec<u32> {
    let prefix = format!("{broker_id}_{day}.");
    let suffix = format!(".{FILE_EXTENSION}");
    let mut blocks: Vec<u32> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let middle = name.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
            middle.parse().ok()
        })
        .collect();
    blocks.sort_unstable();
    blocks
}

fn push_char(bytes: &mut Vec<u8>, character: char) {
    let mut buffer = [0; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

fn escaped(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\r' => line.push_str("\\r"),
            '\t' => line.push('\t'),
            control if control.is_control() => {
                let _ = write!(line, "\\u{{{:x}}}", u32::from(control));
            }
            other => line.push(other),
        }
    }
    line
}

fn unescaped(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        rest = &rest[character.len_utf8()..];
        if character != '\\' {
            push_char(&mut bytes, character);
            continue;
        }
        if let Some(after) = rest.strip_prefix('\\') {
            bytes.push(b'\\');
            rest = after;
        } else if let Some(after) = rest.strip_prefix('n') {
            bytes.push(b'\n');
            rest = after;
        } else if let Some(after) = rest.strip_prefix('r') {
            bytes.push(b'\r');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("u{")
            && let Some((digits, after)) = after.split_once('}')
            && let Some(decoded) = u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32)
        {
            push_char(&mut bytes, decoded);
            rest = after;
        } else {
            bytes.push(b'\\');
        }
    }
    bytes
}

fn encoded_payload(payload: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(payload) else {
        let mut hex = String::with_capacity(2 + payload.len() * 2);
        hex.push_str("0x");
        for byte in payload {
            let _ = write!(hex, "{byte:02x}");
        }
        return hex;
    };
    let ambiguous = text.starts_with("0x")
        || text.starts_with("\\:")
        || text
            .chars()
            .any(|character| character.is_control() && character != '\t');
    if ambiguous {
        format!("\\:{}", escaped(text))
    } else {
        text.to_owned()
    }
}

fn decoded_payload(text: &str) -> Vec<u8> {
    if let Some(hex) = text.strip_prefix("0x")
        && !hex.is_empty()
        && hex.len() % 2 == 0
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return (0..hex.len())
            .step_by(2)
            .filter_map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
            .collect();
    }
    match text.strip_prefix("\\:") {
        Some(escaped_text) => unescaped(escaped_text),
        None => text.as_bytes().to_vec(),
    }
}

fn user_properties(properties: Option<&MessageProperties>) -> String {
    let objects: Vec<serde_json::Value> = properties
        .map(|properties| {
            properties
                .user_properties
                .iter()
                .map(|(key, value)| serde_json::json!({ key: value }))
                .collect()
        })
        .unwrap_or_default();
    serde_json::Value::Array(objects).to_string()
}

fn properties_of(items: Vec<serde_json::Value>) -> Vec<(String, String)> {
    let mut properties = Vec::new();
    for item in items {
        if let serde_json::Value::Object(map) = item {
            for (key, value) in map {
                let text = match value {
                    serde_json::Value::String(text) => text,
                    other => other.to_string(),
                };
                properties.push((key, text));
            }
        }
    }
    properties
}

#[derive(Debug, Clone)]
enum Piece {
    Text(String),
    Time,
    Topic,
    Properties,
    Payload,
}

/// The compiled `[log_format]` settings: writes lines, reads them back and names the day.
#[derive(Debug, Clone)]
pub struct LogFormat {
    pieces: Vec<Piece>,
    time: String,
    utc: bool,
    pattern: Option<Regex>,
}

impl Default for LogFormat {
    fn default() -> Self {
        Self::new(&LogFormatConfig::default()).expect("the default log format is valid")
    }
}

impl LogFormat {
    pub fn new(config: &LogFormatConfig) -> Result<Self> {
        let pieces = parse_template(&config.line)?;
        if !pieces.iter().any(|piece| matches!(piece, Piece::Time))
            || !pieces.iter().any(|piece| matches!(piece, Piece::Topic))
        {
            bail!("log_format.line needs the placeholders {{time}} and {{topic}}");
        }
        if StrftimeItems::new(&config.time).any(|item| matches!(item, chrono::format::Item::Error))
        {
            bail!(
                "log_format.time '{}' is not a valid time format",
                config.time
            );
        }
        let pattern = if config.pattern.trim().is_empty() {
            None
        } else {
            let regex = Regex::new(&config.pattern)
                .with_context(|| "log_format.pattern is not a valid regular expression")?;
            let names: Vec<&str> = regex.capture_names().flatten().collect();
            for needed in ["time", "topic"] {
                if !names.contains(&needed) {
                    bail!("log_format.pattern needs the named group (?P<{needed}>...)");
                }
            }
            Some(regex)
        };
        Ok(Self {
            pieces,
            time: config.time.clone(),
            utc: config.utc,
            pattern,
        })
    }

    fn stamp(&self, wall: SystemTime, format: &str) -> String {
        if self.utc {
            DateTime::<Utc>::from(wall).format(format).to_string()
        } else {
            DateTime::<Local>::from(wall).format(format).to_string()
        }
    }

    /// The day a message belongs to, as it appears in file names.
    pub fn day(&self, wall: SystemTime) -> String {
        self.stamp(wall, "%Y%m%d")
    }

    /// One log line without the trailing newline.
    pub fn format_line(
        &self,
        wall: SystemTime,
        topic: &str,
        payload: &[u8],
        properties: Option<&MessageProperties>,
    ) -> String {
        let mut line = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(text) => line.push_str(text),
                Piece::Time => line.push_str(&self.stamp(wall, &self.time)),
                Piece::Topic => line.push_str(topic),
                Piece::Properties => line.push_str(&user_properties(properties)),
                Piece::Payload => line.push_str(&encoded_payload(payload)),
            }
        }
        if payload.is_empty() {
            line.truncate(line.trim_end_matches(' ').len());
        }
        line
    }

    /// Reads a line written with this format (or, without a `pattern`, the default layout).
    pub fn parse_line(&self, line: &str) -> Option<LogRecord> {
        let Some(pattern) = &self.pattern else {
            return parse_default(line);
        };
        let captures = pattern.captures(line)?;
        let time =
            NaiveDateTime::parse_from_str(captures.name("time")?.as_str(), &self.time).ok()?;
        let topic = captures.name("topic")?.as_str();
        if topic.is_empty() {
            return None;
        }
        let user_properties = match captures.name("properties") {
            Some(text) => match serde_json::from_str(text.as_str()) {
                Ok(serde_json::Value::Array(items)) => properties_of(items),
                _ => return None,
            },
            None => Vec::new(),
        };
        Some(LogRecord {
            millis: time.and_utc().timestamp_millis(),
            topic: topic.to_owned(),
            payload: captures
                .name("payload")
                .map_or_else(Vec::new, |text| decoded_payload(text.as_str())),
            user_properties,
        })
    }
}

fn parse_template(template: &str) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                text.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                text.push('}');
            }
            '{' => {
                let name: String = characters.by_ref().take_while(|c| *c != '}').collect();
                let piece = match name.as_str() {
                    "time" => Piece::Time,
                    "topic" => Piece::Topic,
                    "properties" => Piece::Properties,
                    "payload" => Piece::Payload,
                    other => bail!("log_format.line has an unknown placeholder {{{other}}}"),
                };
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(piece);
            }
            '}' => bail!("log_format.line has a lone '}}'; write '}}}}' for a brace"),
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// A message read back from a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// Local wall-clock time as milliseconds since 1970 as if it were UTC; only differences matter.
    pub millis: i64,
    pub topic: String,
    pub payload: Vec<u8>,
    pub user_properties: Vec<(String, String)>,
}

/// The built-in reader for the default layout; tolerant of the thread and level fields.
fn parse_default(line: &str) -> Option<LogRecord> {
    let stamp = line.get(..19)?;
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let fraction = line.get(19..23)?.strip_prefix(',')?;
    if fraction.len() != 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let millis = naive.and_utc().timestamp_millis() + fraction.parse::<i64>().ok()?;

    let rest = line.get(23..)?.strip_prefix(" | ")?;
    let (thread, rest) = rest.split_once(' ')?;
    if !thread.starts_with('T') {
        return None;
    }
    let rest = rest.trim_start();
    let mut level = rest.chars();
    let (marker, colon) = (level.next()?, level.next()?);
    if !marker.is_ascii_alphabetic() || colon != ':' {
        return None;
    }
    let tail = rest.get(2..)?.strip_prefix(' ')?;

    // The topic may contain " [" itself, so take the first one that is followed by a JSON array.
    for (at, _) in tail.match_indices(" [") {
        let topic = &tail[..at];
        if topic.is_empty() {
            continue;
        }
        let candidate = &tail[at + 1..];
        let mut stream =
            serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();
        let Some(Ok(serde_json::Value::Array(items))) = stream.next() else {
            continue;
        };
        let after = &candidate[stream.byte_offset()..];
        let payload = match after.strip_prefix(' ') {
            Some(text) => decoded_payload(text),
            None if after.is_empty() => Vec::new(),
            None => continue,
        };
        return Some(LogRecord {
            millis,
            topic: topic.to_owned(),
            payload,
            user_properties: properties_of(items),
        });
    }
    None
}

struct Output {
    writer: BufWriter<File>,
    day: String,
    index: u32,
    bytes: u64,
}

/// Appends lines to the per-broker files of one directory; files open on first use.
#[derive(Default)]
pub struct MqttLogger {
    format: LogFormat,
    directory: Option<PathBuf>,
    outputs: HashMap<String, Output>,
    unflushed: bool,
}

impl MqttLogger {
    pub fn new(format: LogFormat) -> Self {
        let mut logger = Self::default();
        logger.format = format;
        logger
    }

    /// Appends one message to the broker's current file, starting a new one for a new day or
    /// when the line would take the file beyond `max_bytes`.
    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &mut self,
        directory: &Path,
        max_bytes: u64,
        broker_id: &str,
        wall: SystemTime,
        topic: &str,
        payload: &[u8],
        properties: Option<&MessageProperties>,
    ) -> Result<()> {
        if self.directory.as_deref() != Some(directory) {
            self.close();
            self.directory = Some(directory.to_path_buf());
        }
        let line = self.format.format_line(wall, topic, payload, properties);
        let needed = line.len() as u64 + 1;
        let day = self.format.day(wall);
        let open_from = match self.outputs.get(broker_id) {
            Some(output) if output.day == day => {
                (output.bytes > 0 && output.bytes + needed > max_bytes).then_some(output.index + 1)
            }
            _ => Some(0),
        };
        if let Some(from) = open_from {
            if let Some(mut old) = self.outputs.remove(broker_id) {
                let _ = old.writer.flush();
            }
            let output = open_block(directory, broker_id, &day, from, needed, max_bytes)?;
            self.outputs.insert(broker_id.to_owned(), output);
        }
        let Some(output) = self.outputs.get_mut(broker_id) else {
            return Ok(());
        };
        writeln!(output.writer, "{line}").with_context(|| {
            format!(
                "could not write {}",
                directory
                    .join(file_name(broker_id, &output.day, output.index))
                    .display()
            )
        })?;
        output.bytes += needed;
        self.unflushed = true;
        Ok(())
    }

    /// Writes buffered lines out; call once per event-loop iteration.
    pub fn flush(&mut self) -> Result<()> {
        if !std::mem::take(&mut self.unflushed) {
            return Ok(());
        }
        for output in self.outputs.values_mut() {
            output
                .writer
                .flush()
                .context("could not write the log file")?;
        }
        Ok(())
    }

    /// Flushes and closes every file, for example when logging is switched off.
    pub fn close(&mut self) {
        for output in self.outputs.values_mut() {
            let _ = output.writer.flush();
        }
        self.outputs.clear();
        self.unflushed = false;
    }
}

impl Drop for MqttLogger {
    fn drop(&mut self) {
        self.close();
    }
}

/// Opens the first block of the day that has room for `needed` bytes. `from` 0 starts at the
/// newest existing block (a restart continues it); a higher number is the block after a full one.
fn open_block(
    directory: &Path,
    broker_id: &str,
    day: &str,
    from: u32,
    needed: u64,
    max_bytes: u64,
) -> Result<Output> {
    fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let mut index = if from == 0 {
        existing_blocks(directory, broker_id, day)
            .last()
            .copied()
            .unwrap_or(0)
    } else {
        from
    };
    loop {
        let path = directory.join(file_name(broker_id, day, index));
        let size = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        if size == 0 || size + needed <= max_bytes {
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            // Payloads can be confidential.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(&path)
                .with_context(|| format!("could not open {}", path.display()))?;
            return Ok(Output {
                writer: BufWriter::new(file),
                day: day.to_owned(),
                index,
                bytes: size,
            });
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_761_000_000_353)
    }

    fn fmt() -> LogFormat {
        LogFormat::default()
    }

    fn timestamp_len() -> usize {
        "2026-10-23 04:32:28,353".len()
    }

    fn props(pairs: &[(&str, &str)]) -> MessageProperties {
        MessageProperties {
            user_properties: pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            ..MessageProperties::default()
        }
    }

    fn lines_of(directory: &Path, broker: &str, index: u32) -> usize {
        fs::read_to_string(directory.join(file_name(broker, &fmt().day(wall()), index)))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn lines_follow_the_documented_layout() {
        let properties = props(&[("source", "plc \"1\"")]);
        let line = fmt().format_line(wall(), "site1/setup", br#"{"a":1}"#, Some(&properties));
        assert_eq!(
            &line[timestamp_len()..],
            r#" | T0   I: site1/setup [{"source":"plc \"1\""}] {"a":1}"#
        );
        assert_eq!(&line[4..5], "-");
        assert_eq!(&line[19..20], ",", "milliseconds follow a comma");
    }

    #[test]
    fn missing_properties_and_empty_payloads_stay_well_formed() {
        let line = fmt().format_line(wall(), "t", b"", None);
        assert!(line.ends_with(" | T0   I: t []"), "{line}");
        let line = fmt().format_line(wall(), "t", b"x", Some(&MessageProperties::default()));
        assert!(line.ends_with(" | T0   I: t [] x"), "{line}");
    }

    #[test]
    fn payloads_stay_on_one_line_and_binary_is_hex() {
        let line = fmt().format_line(wall(), "t", b"a\r\nb\x01\tc", None);
        assert!(!line.contains(['\n', '\r']));
        assert!(line.ends_with("[] \\:a\\r\\nb\\u{1}\tc"), "{line}");
        let line = fmt().format_line(wall(), "t", &[0xff, 0x00, 0x41], None);
        assert!(line.ends_with(" [] 0xff0041"), "{line}");
        let line = fmt().format_line(wall(), "t", br#"{"a":"x\ny"}"#, None);
        assert!(
            line.ends_with(r#"[] {"a":"x\ny"}"#),
            "JSON escapes stay as they are: {line}"
        );
    }

    #[test]
    fn every_payload_form_reads_back_exactly() {
        let samples: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"plain text".to_vec(),
            br#"{"a":"x\ny","b":[1,2]}"#.to_vec(),
            b"{\n  \"pretty\": true\n}\n".to_vec(),
            b"back\\slash and \\n literal".to_vec(),
            b"0x1234 is text".to_vec(),
            b"0x".to_vec(),
            b"\\:starts like the marker".to_vec(),
            b" leading and trailing ".to_vec(),
            b"tab\there".to_vec(),
            "umlaut \u{e4} \u{1f600}".as_bytes().to_vec(),
            vec![0xff, 0x00, 0x41, 0x80],
            vec![0x00],
            "x\u{7f}y\u{85}z".as_bytes().to_vec(),
        ];
        for payload in samples {
            let line = fmt().format_line(wall(), "a b/c [d]", &payload, None);
            assert!(!line.contains(['\n', '\r']), "{line:?}");
            let record = fmt()
                .parse_line(&line)
                .unwrap_or_else(|| panic!("unparsable: {line:?}"));
            assert_eq!(record.payload, payload, "{line:?}");
            assert_eq!(record.topic, "a b/c [d]");
            assert!(record.user_properties.is_empty());
        }
    }

    #[test]
    fn user_properties_time_and_foreign_logs_are_parsed() {
        let properties = props(&[("source", "plc \"1\""), ("n", "2")]);
        let line = fmt().format_line(wall(), "t/x", b"v", Some(&properties));
        let record = fmt().parse_line(&line).unwrap();
        assert_eq!(record.user_properties, properties.user_properties);
        let utc = DateTime::<Utc>::from(wall()).naive_utc();
        assert_eq!(record.millis, utc.and_utc().timestamp_millis());

        let foreign = r#"2026-10-23 04:32:28,353 | T45  I: site1/setup/plc/base_par/rq/PRESS1 [{"source":"Example.Unit"}] {"header":{"sequence":1},"data":{}}"#;
        let record = fmt().parse_line(foreign).unwrap();
        assert_eq!(record.topic, "site1/setup/plc/base_par/rq/PRESS1");
        assert_eq!(record.payload, br#"{"header":{"sequence":1},"data":{}}"#);
        assert_eq!(
            record.user_properties,
            [("source".to_owned(), "Example.Unit".to_owned())]
        );
        let next = fmt()
            .parse_line("2026-10-23 04:32:29,353 | T3  I: t [] x")
            .unwrap();
        assert_eq!(next.millis - record.millis, 1000);
    }

    #[test]
    fn lines_that_are_not_messages_are_rejected() {
        for line in [
            "",
            "garbage",
            "2026-10-23 04:32:28,353",
            "2026-10-23 04:32:28,353 | T0  I: [] no topic",
            "2026-10-23 04:32:28,353 | T0  I: topic without properties",
            "2026-10-23 04:32:28,35 | T0  I: t []",
            "2026-13-23 04:32:28,353 | T0  I: t []",
            "2026-10-23 04:32:28,353 | T0  II t []",
        ] {
            assert!(fmt().parse_line(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn each_broker_gets_its_own_files_named_by_day_and_block() {
        let directory = tempfile::tempdir().unwrap();
        let mut logger = MqttLogger::default();
        let max = 1 << 20;
        let log = |logger: &mut MqttLogger, broker: &str, topic: &str, payload: &[u8]| {
            logger
                .log(directory.path(), max, broker, wall(), topic, payload, None)
                .unwrap();
        };
        log(&mut logger, "alpha", "a/b", b"1");
        log(&mut logger, "beta", "c", b"2");
        log(&mut logger, "alpha", "a/b", b"3");
        logger.flush().unwrap();
        assert_eq!(lines_of(directory.path(), "alpha", 0), 2);
        assert_eq!(lines_of(directory.path(), "beta", 0), 1);
        let text = fs::read_to_string(directory.path().join(file_name(
            "alpha",
            &fmt().day(wall()),
            0,
        )))
        .unwrap();
        assert!(text.lines().last().unwrap().ends_with("I: a/b [] 3"));

        drop(logger);
        let mut again = MqttLogger::default();
        log(&mut again, "alpha", "x", b"4");
        again.close();
        assert_eq!(
            lines_of(directory.path(), "alpha", 0),
            3,
            "existing files are appended to"
        );
    }

    #[test]
    fn a_new_block_starts_before_a_file_would_exceed_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let one_line = fmt().format_line(wall(), "t", b"0123456789", None).len() as u64 + 1;
        let max = one_line * 2 + one_line / 2;
        let mut logger = MqttLogger::default();
        for _ in 0..5 {
            logger
                .log(directory.path(), max, "a", wall(), "t", b"0123456789", None)
                .unwrap();
        }
        logger.close();
        let lines = |index| lines_of(directory.path(), "a", index);
        assert_eq!((lines(0), lines(1), lines(2)), (2, 2, 1));
        for index in 0..3 {
            let path = directory
                .path()
                .join(file_name("a", &fmt().day(wall()), index));
            assert!(fs::metadata(path).unwrap().len() <= max);
        }

        // After a restart the newest block with room is continued, a full one is skipped.
        let mut again = MqttLogger::default();
        for _ in 0..2 {
            again
                .log(directory.path(), max, "a", wall(), "t", b"0123456789", None)
                .unwrap();
        }
        again.close();
        assert_eq!((lines(2), lines(3)), (2, 1));
    }

    #[test]
    fn a_line_larger_than_the_limit_gets_a_block_of_its_own() {
        let directory = tempfile::tempdir().unwrap();
        let mut logger = MqttLogger::default();
        let big = vec![b'x'; 500];
        for payload in [&b"s"[..], &big, b"s"] {
            logger
                .log(directory.path(), 100, "a", wall(), "t", payload, None)
                .unwrap();
        }
        logger.close();
        for index in 0..3 {
            assert_eq!(lines_of(directory.path(), "a", index), 1, "block {index}");
        }
    }

    #[test]
    fn a_new_day_starts_at_block_zero() {
        let directory = tempfile::tempdir().unwrap();
        let mut logger = MqttLogger::default();
        let next_day = wall() + std::time::Duration::from_secs(3 * 24 * 3600);
        for at in [wall(), next_day] {
            logger
                .log(directory.path(), 1 << 20, "a", at, "t", b"1", None)
                .unwrap();
        }
        logger.close();
        for at in [wall(), next_day] {
            assert!(
                directory
                    .path()
                    .join(file_name("a", &fmt().day(at), 0))
                    .exists()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn log_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let mut logger = MqttLogger::default();
        logger
            .log(directory.path(), 1 << 20, "alpha", wall(), "t", b"", None)
            .unwrap();
        let path = directory
            .path()
            .join(file_name("alpha", &fmt().day(wall()), 0));
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn switching_the_directory_starts_new_files() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut logger = MqttLogger::default();
        for directory in [first.path(), second.path()] {
            logger
                .log(directory, 1 << 20, "a", wall(), "t", b"1", None)
                .unwrap();
        }
        logger.close();
        assert_eq!(lines_of(first.path(), "a", 0), 1);
        assert_eq!(lines_of(second.path(), "a", 0), 1);
    }

    #[test]
    fn the_default_layout_matches_the_reference_logs() {
        // From a log of the tool this format is modelled on; its thread field is padded to four characters.
        let sample = r#"2026-10-29 07:09:55,136 | T96  I: site1/trace/request/PRESS1.trace_01_01 [{"gear":"admin"},{"$bridge":"edge-broker"}] {"header":{"sequence":48479},"data":{}}"#;
        let record = fmt().parse_line(sample).unwrap();
        assert_eq!(record.user_properties.len(), 2);
        assert_eq!(record.user_properties[1].0, "$bridge");
        let wall =
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis((record.millis) as u64);
        let properties = props(&[("gear", "admin"), ("$bridge", "edge-broker")]);
        let line = fmt().format_line(wall, &record.topic, &record.payload, Some(&properties));
        assert_eq!(line, sample.replacen("T96  I:", "T0   I:", 1));

        for sample in [
            r#"2026-10-29 05:47:28,076 | T82  I: site0/storage/rq_all/response [] {"instancename":"StorageService_01_01"}"#,
            r#"2026-10-29 07:47:35,279 | T110 I: site1/machine/state/PRESS1_X [{"source":"plc1.ctrl.connect"}] {"data":{"state":1}}"#,
        ] {
            let record = fmt().parse_line(sample).unwrap();
            assert!(record.topic.starts_with("site"), "{sample}");
            assert!(record.payload.starts_with(b"{"), "{sample}");
        }
    }

    #[test]
    fn times_are_utc_unless_configured_otherwise() {
        let line = fmt().format_line(wall(), "t", b"", None);
        assert!(line.starts_with("2025-10-20 22:40:00,353 |"), "{line}");
        assert_eq!(fmt().day(wall()), "20251020");
        let local = LogFormat::new(&LogFormatConfig {
            utc: false,
            ..LogFormatConfig::default()
        })
        .unwrap();
        let expected = DateTime::<Local>::from(wall()).format("%Y%m%d").to_string();
        assert_eq!(local.day(wall()), expected);
    }

    #[test]
    fn a_custom_layout_writes_and_reads_back_through_its_pattern() {
        let format = LogFormat::new(&LogFormatConfig {
            line: "[{time}] {topic} => {payload} {{{properties}}}".to_owned(),
            time: "%d.%m.%Y %H:%M:%S".to_owned(),
            utc: true,
            pattern: r"^\[(?P<time>[^\]]+)\] (?P<topic>\S+) => (?P<payload>.*) \{(?P<properties>\[.*\])\}$"
                .to_owned(),
        })
        .unwrap();
        let properties = props(&[("k", "v")]);
        let line = format.format_line(wall(), "a/b", b"hello world", Some(&properties));
        assert_eq!(
            line,
            r#"[20.10.2025 22:40:00] a/b => hello world {[{"k":"v"}]}"#
        );
        let record = format.parse_line(&line).unwrap();
        assert_eq!(
            (record.topic.as_str(), record.payload.as_slice()),
            ("a/b", &b"hello world"[..])
        );
        assert_eq!(record.user_properties, properties.user_properties);
        assert!(format.parse_line("not in this layout").is_none());

        // Without properties and payload groups a pattern still yields the message.
        let minimal = LogFormat::new(&LogFormatConfig {
            pattern: r"^(?P<time>\S+ \S+) \| (?P<topic>\S+)".to_owned(),
            time: "%Y-%m-%d %H:%M:%S,%3f".to_owned(),
            ..LogFormatConfig::default()
        })
        .unwrap();
        let record = minimal
            .parse_line("2026-10-29 05:47:28,076 | a/b anything")
            .unwrap();
        assert_eq!((record.topic.as_str(), record.payload.len()), ("a/b", 0));
    }

    #[test]
    fn invalid_formats_are_refused_with_a_reason() {
        let bad = |change: fn(&mut LogFormatConfig)| {
            let mut config = LogFormatConfig::default();
            change(&mut config);
            LogFormat::new(&config).unwrap_err().to_string()
        };
        assert!(bad(|c| c.line = "{time} {topic} {nope}".into()).contains("{nope}"));
        assert!(bad(|c| c.line = "{time} {payload}".into()).contains("{topic}"));
        assert!(bad(|c| c.line = "{time} {topic} }".into()).contains("lone"));
        assert!(bad(|c| c.time = "%Q".into()).contains("time format"));
        assert!(bad(|c| c.pattern = "(".into()).contains("regular expression"));
        assert!(bad(|c| c.pattern = r"^(?P<time>.*)$".into()).contains("topic"));
    }
}
