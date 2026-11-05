//! Short, single-line payload summaries for the topic explorer.

use serde_json::Value;

/// Larger JSON-looking payloads are classified by their first character instead of being parsed.
const MAX_PARSED_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Empty,
    Text,
    Number,
    Bool,
    Null,
    Object,
    Array,
    Binary,
    /// Looks like JSON but does not parse.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    pub kind: PreviewKind,
}

impl Preview {
    fn new(kind: PreviewKind, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

pub fn payload_preview(payload: &[u8]) -> Preview {
    if payload.is_empty() {
        return Preview::new(PreviewKind::Empty, "(empty)");
    }
    let Ok(text) = std::str::from_utf8(payload) else {
        return Preview::new(PreviewKind::Binary, format!("<{} bytes>", payload.len()));
    };
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Preview::new(PreviewKind::Binary, format!("<{} bytes>", payload.len()));
    }

    let trimmed = text.trim();
    match trimmed.chars().next() {
        Some(opening @ ('{' | '[')) => {
            let kind = if opening == '{' {
                PreviewKind::Object
            } else {
                PreviewKind::Array
            };
            if payload.len() > MAX_PARSED_BYTES {
                return Preview::new(kind, if opening == '{' { "{…}" } else { "[…]" });
            }
            return match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(_)) => Preview::new(PreviewKind::Object, "{…}"),
                Ok(Value::Array(items)) => Preview::new(
                    PreviewKind::Array,
                    if items.len() == 1 {
                        "[1 item]".to_owned()
                    } else {
                        format!("[{} items]", items.len())
                    },
                ),
                _ => Preview::new(PreviewKind::Invalid, single_line(trimmed)),
            };
        }
        None => return Preview::new(PreviewKind::Empty, "(blank)"),
        _ => {}
    }

    if trimmed.len() <= 64
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        match value {
            Value::Number(_) => return Preview::new(PreviewKind::Number, trimmed),
            Value::Bool(_) => return Preview::new(PreviewKind::Bool, trimmed),
            Value::Null => return Preview::new(PreviewKind::Null, trimmed),
            _ => {}
        }
    }
    Preview::new(PreviewKind::Text, single_line(trimmed))
}

fn single_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    let multiline = lines.next().is_some();
    let mut result: String = first.chars().take(MAX_PREVIEW_CHARS).collect();
    if multiline || first.chars().count() > MAX_PREVIEW_CHARS {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(payload: &str) -> (PreviewKind, String) {
        let preview = payload_preview(payload.as_bytes());
        (preview.kind, preview.text)
    }

    #[test]
    fn scalars_are_shown_directly() {
        assert_eq!(preview("RUNNING"), (PreviewKind::Text, "RUNNING".into()));
        assert_eq!(preview("42.5"), (PreviewKind::Number, "42.5".into()));
        assert_eq!(preview("true"), (PreviewKind::Bool, "true".into()));
        assert_eq!(preview("null"), (PreviewKind::Null, "null".into()));
        assert_eq!(preview("\"hi\""), (PreviewKind::Text, "\"hi\"".into()));
    }

    #[test]
    fn json_containers_are_summarised_not_inlined() {
        assert_eq!(
            preview(r#"{"a":{"b":1}}"#),
            (PreviewKind::Object, "{…}".into())
        );
        assert_eq!(preview("[1,2,3]"), (PreviewKind::Array, "[3 items]".into()));
        assert_eq!(preview("[1]"), (PreviewKind::Array, "[1 item]".into()));
    }

    #[test]
    fn broken_json_binary_and_empty_payloads_are_classified() {
        assert_eq!(preview("{oops").0, PreviewKind::Invalid);
        assert_eq!(
            payload_preview(&[0xff, 0xfe, 0x00]).kind,
            PreviewKind::Binary
        );
        assert_eq!(payload_preview(b"\x01\x02").text, "<2 bytes>");
        assert_eq!(payload_preview(b"").kind, PreviewKind::Empty);
    }

    #[test]
    fn multiline_and_long_text_is_cut_to_one_line() {
        assert_eq!(preview("first\nsecond").1, "first…");
        let long = "x".repeat(300);
        assert_eq!(preview(&long).1.chars().count(), MAX_PREVIEW_CHARS + 1);
    }
}
