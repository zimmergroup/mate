//! Substring search over a message's payload and MQTT 5 properties.

use crate::topics::{Message, MessageProperties};

/// Only this much of a payload is searched, so a huge message cannot stall the screen.
pub const MAX_SEARCH_BYTES: usize = 64 * 1024;

/// Case-insensitive for ASCII; other characters must match exactly.
pub fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Matches the payload, content type, response topic, correlation data and user properties
/// (as key, value or `key=value`). An empty query matches everything.
pub fn message_matches(message: &Message, query: &str) -> bool {
    content_matches(&message.payload, message.properties.as_deref(), query)
}

/// Like [`message_matches`] for a payload and properties that are not stored as a [`Message`].
pub fn content_matches(
    payload: &[u8],
    properties: Option<&MessageProperties>,
    query: &str,
) -> bool {
    let needle = query.as_bytes();
    if needle.is_empty() {
        return true;
    }
    let payload = &payload[..payload.len().min(MAX_SEARCH_BYTES)];
    if contains_ignore_ascii_case(payload, needle) {
        return true;
    }
    let Some(properties) = properties else {
        return false;
    };
    let hit = |text: &str| contains_ignore_ascii_case(text.as_bytes(), needle);
    properties.content_type.as_deref().is_some_and(hit)
        || properties.response_topic.as_deref().is_some_and(hit)
        || properties
            .correlation_data
            .as_deref()
            .is_some_and(|data| contains_ignore_ascii_case(data, needle))
        || properties
            .user_properties
            .iter()
            .any(|(key, value)| hit(key) || hit(value) || hit(&format!("{key}={value}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topics::{MessageMeta, TopicStore};
    use std::sync::Arc;

    fn stored(payload: &[u8], properties: Option<MessageProperties>) -> Message {
        let mut topics = TopicStore::default();
        let mut meta = MessageMeta::now();
        meta.properties = properties.map(Arc::new);
        topics.insert_message("t", payload.to_vec(), meta);
        topics.latest_message("t").unwrap().clone()
    }

    #[test]
    fn payload_matches_ignore_ascii_case_and_empty_queries_match_everything() {
        let message = stored(b"{\"State\":\"RUNNING\"}", None);
        assert!(message_matches(&message, "running"));
        assert!(message_matches(&message, "\"state\""));
        assert!(!message_matches(&message, "stopped"));
        assert!(message_matches(&message, ""));
        assert!(!message_matches(&stored(b"ab", None), "abc"));
    }

    #[test]
    fn properties_match_by_key_value_or_pair() {
        let properties = MessageProperties {
            content_type: Some("application/json".to_owned()),
            response_topic: Some("reply/plc-7".to_owned()),
            correlation_data: Some(b"req-42".to_vec()),
            user_properties: vec![("origin".to_owned(), "PLC-7".to_owned())],
            ..MessageProperties::default()
        };
        let message = stored(b"x", Some(properties));
        for query in [
            "origin=plc-7",
            "ORIGIN",
            "plc-7",
            "application/json",
            "reply/",
            "req-42",
        ] {
            assert!(message_matches(&message, query), "{query}");
        }
        assert!(!message_matches(&message, "origin=hall"));
        assert!(!message_matches(&stored(b"x", None), "origin"));
    }
}
