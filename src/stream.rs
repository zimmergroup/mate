//! A bounded, chronological log of received messages and connection events.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::topics::MessageProperties;

pub const MAX_ENTRIES: usize = 50_000;
/// Payloads are shared with the topic history, but the log keeps evicted ones alive, so it has its own budget.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct StreamMessage {
    pub topic: String,
    pub payload: Arc<[u8]>,
    pub qos: u8,
    pub retain: bool,
    pub dup: bool,
    pub properties: Option<Arc<MessageProperties>>,
    /// Time since the previous message on the same topic.
    pub since_previous: Option<Duration>,
    /// ID of the broker that delivered it; `None` when only one broker is involved.
    pub broker: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
pub enum StreamKind {
    Message(StreamMessage),
    Event { text: String, error: bool },
}

#[derive(Debug, Clone)]
pub struct StreamEntry {
    /// Grows by one per entry and is never reused, so it identifies an entry across evictions.
    pub id: u64,
    pub wall: SystemTime,
    pub kind: StreamKind,
}

#[derive(Debug)]
pub struct StreamLog {
    entries: VecDeque<StreamEntry>,
    next_id: u64,
    payload_bytes: usize,
    dropped: u64,
    max_entries: usize,
    max_payload_bytes: usize,
}

impl Default for StreamLog {
    fn default() -> Self {
        Self::with_limits(MAX_ENTRIES, MAX_PAYLOAD_BYTES)
    }
}

fn payload_len(kind: &StreamKind) -> usize {
    match kind {
        StreamKind::Message(message) => message.payload.len(),
        StreamKind::Event { .. } => 0,
    }
}

impl StreamLog {
    pub fn with_limits(max_entries: usize, max_payload_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 0,
            payload_bytes: 0,
            dropped: 0,
            max_entries,
            max_payload_bytes,
        }
    }

    pub fn push_message(&mut self, wall: SystemTime, message: StreamMessage) {
        self.push(wall, StreamKind::Message(message));
    }

    pub fn push_event(&mut self, wall: SystemTime, text: impl Into<String>, error: bool) {
        self.push(
            wall,
            StreamKind::Event {
                text: text.into(),
                error,
            },
        );
    }

    fn push(&mut self, wall: SystemTime, kind: StreamKind) {
        self.payload_bytes += payload_len(&kind);
        self.entries.push_back(StreamEntry {
            id: self.next_id,
            wall,
            kind,
        });
        self.next_id += 1;
        while self.entries.len() > 1
            && (self.entries.len() > self.max_entries
                || self.payload_bytes > self.max_payload_bytes)
        {
            if let Some(oldest) = self.entries.pop_front() {
                self.payload_bytes -= payload_len(&oldest.kind);
                self.dropped += 1;
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.payload_bytes = 0;
        self.dropped = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted because of the limits since the log was last cleared.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn first_id(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.id)
    }

    pub fn last_id(&self) -> Option<u64> {
        self.entries.back().map(|entry| entry.id)
    }

    pub fn get(&self, id: u64) -> Option<&StreamEntry> {
        let first = self.first_id()?;
        let index = usize::try_from(id.checked_sub(first)?).ok()?;
        self.entries.get(index)
    }

    /// Entries with an id greater than `after` (all of them for `None`), oldest first.
    pub fn since(&self, after: Option<u64>) -> impl Iterator<Item = &StreamEntry> {
        let first = self.first_id().unwrap_or(0);
        let skip = after.map_or(0, |after| {
            usize::try_from((after + 1).saturating_sub(first)).unwrap_or(usize::MAX)
        });
        self.entries.iter().skip(skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: &str, size: usize) -> StreamMessage {
        StreamMessage {
            topic: topic.to_owned(),
            payload: vec![0u8; size].into(),
            qos: 0,
            retain: false,
            dup: false,
            properties: None,
            since_previous: None,
            broker: None,
        }
    }

    fn topic_of(entry: &StreamEntry) -> &str {
        match &entry.kind {
            StreamKind::Message(message) => &message.topic,
            StreamKind::Event { text, .. } => text,
        }
    }

    #[test]
    fn old_entries_are_dropped_and_counted_when_the_entry_limit_is_exceeded() {
        let mut log = StreamLog::with_limits(3, usize::MAX);
        for index in 0..5 {
            log.push_message(SystemTime::now(), message(&format!("t{index}"), 1));
        }
        assert_eq!((log.len(), log.dropped()), (3, 2));
        assert_eq!((log.first_id(), log.last_id()), (Some(2), Some(4)));
        assert_eq!(topic_of(log.get(2).unwrap()), "t2");
        assert!(log.get(1).is_none() && log.get(5).is_none());
    }

    #[test]
    fn the_payload_budget_evicts_the_oldest_entries_but_keeps_the_newest() {
        let mut log = StreamLog::with_limits(100, 10);
        log.push_message(SystemTime::now(), message("a", 6));
        log.push_message(SystemTime::now(), message("b", 6));
        assert_eq!(log.len(), 1, "6 + 6 bytes exceed the budget of 10");
        log.push_event(SystemTime::now(), "Connected", false);
        assert_eq!(log.len(), 2, "events carry no payload");
        log.push_message(SystemTime::now(), message("huge", 50));
        assert_eq!(log.len(), 1, "an oversized entry stays alone");
        assert_eq!(topic_of(log.get(log.last_id().unwrap()).unwrap()), "huge");
    }

    #[test]
    fn since_returns_only_newer_entries_even_after_evictions() {
        let mut log = StreamLog::with_limits(3, usize::MAX);
        for index in 0..6 {
            log.push_message(SystemTime::now(), message(&format!("t{index}"), 0));
        }
        let ids = |after| log.since(after).map(|entry| entry.id).collect::<Vec<_>>();
        assert_eq!(ids(None), [3, 4, 5]);
        assert_eq!(ids(Some(3)), [4, 5]);
        assert_eq!(
            ids(Some(0)),
            [3, 4, 5],
            "an evicted id yields everything left"
        );
        assert!(ids(Some(5)).is_empty());
    }

    #[test]
    fn clear_forgets_entries_but_ids_keep_growing() {
        let mut log = StreamLog::default();
        log.push_event(SystemTime::now(), "x", false);
        log.clear();
        assert!(log.is_empty() && log.dropped() == 0);
        log.push_event(SystemTime::now(), "y", true);
        assert_eq!(log.first_id(), Some(1));
    }
}
