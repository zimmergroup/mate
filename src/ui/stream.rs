//! The stream view: every received message and connection event in arrival order, with expandable details.

use std::collections::{BTreeSet, HashMap};

use crossterm::event::KeyCode;
use mate::{
    fuzzy::match_positions,
    search::content_matches,
    stream::{StreamEntry, StreamKind, StreamLog, StreamMessage},
};
use serde_json::Value;

use super::{
    UiAction, UiState, View, analytics::payload_preview, info::duration_text, layout::Panel,
    render::format_size,
};

const MAX_DETAIL_LINES: usize = 40;
const MAX_PRETTY_BYTES: usize = 256 * 1024;
pub const PREVIEW_CHARS: usize = 160;

/// One line below an expanded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailLine {
    Info(String),
    Property(String),
    Json(String),
    Text(String),
    Note(String),
}

fn printable(line: &str) -> String {
    line.chars()
        .map(|character| match character {
            '\t' => ' ',
            character if character.is_control() => '\u{b7}',
            character => character,
        })
        .collect()
}

fn hex_lines(payload: &[u8]) -> Vec<String> {
    payload
        .chunks(16)
        .enumerate()
        .map(|(index, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        *byte as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:04x}  {:<47}  {ascii}", index * 16, hex.join(" "))
        })
        .collect()
}

/// The facts, properties and payload shown when an entry is expanded.
pub fn detail_lines(message: &StreamMessage) -> Vec<DetailLine> {
    let mut lines = Vec::new();
    let since = message.since_previous.map_or_else(
        || "first message seen on this topic".to_owned(),
        |gap| {
            format!(
                "{} after the previous one on this topic",
                duration_text(gap)
            )
        },
    );
    lines.push(DetailLine::Info(format!(
        "QoS {}{}{} \u{b7} {} \u{b7} {since}",
        message.qos,
        if message.retain {
            " \u{b7} retained"
        } else {
            ""
        },
        if message.dup { " \u{b7} duplicate" } else { "" },
        format_size(message.payload.len()),
    )));
    if let Some(properties) = message.properties.as_deref() {
        let mut property = |text: String| lines.push(DetailLine::Property(text));
        if let Some(content_type) = &properties.content_type {
            property(format!("content type: {content_type}"));
        }
        match properties.payload_is_utf8 {
            Some(true) => property("payload format: UTF-8 text".to_owned()),
            Some(false) => property("payload format: unspecified bytes".to_owned()),
            None => {}
        }
        if let Some(seconds) = properties.message_expiry_secs {
            property(format!("expiry: {seconds} s"));
        }
        if let Some(topic) = &properties.response_topic {
            property(format!("response topic: {topic}"));
        }
        if let Some(data) = &properties.correlation_data {
            let text = String::from_utf8(data.clone()).map_or_else(
                |_| data.iter().map(|byte| format!("{byte:02x}")).collect(),
                |text| printable(&text),
            );
            property(format!("correlation data: {text}"));
        }
        if !properties.subscription_ids.is_empty() {
            let ids: Vec<String> = properties
                .subscription_ids
                .iter()
                .map(ToString::to_string)
                .collect();
            property(format!("subscription ids: {}", ids.join(", ")));
        }
        for (key, value) in &properties.user_properties {
            property(format!("{} = {}", printable(key), printable(value)));
        }
    }

    let payload: &[u8] = &message.payload;
    let body: Vec<DetailLine> = if payload.is_empty() {
        vec![DetailLine::Note("(empty payload)".to_owned())]
    } else if let Some(pretty) = (payload.len() <= MAX_PRETTY_BYTES)
        .then(|| serde_json::from_slice::<Value>(payload).ok())
        .flatten()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
    {
        pretty
            .lines()
            .map(|line| DetailLine::Json(line.to_owned()))
            .collect()
    } else if let Ok(text) = std::str::from_utf8(payload) {
        text.lines()
            .map(|line| DetailLine::Text(printable(line)))
            .collect()
    } else {
        hex_lines(payload)
            .into_iter()
            .map(DetailLine::Text)
            .collect()
    };
    let extra = body.len().saturating_sub(MAX_DETAIL_LINES);
    lines.extend(body.into_iter().take(MAX_DETAIL_LINES));
    if extra > 0 {
        lines.push(DetailLine::Note(format!(
            "\u{2026} {extra} more lines (c copies the whole payload)"
        )));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Topic,
    Text,
}

#[derive(Debug)]
pub struct StreamView {
    /// The selection sticks to the newest entry.
    pub follow: bool,
    pub paused: bool,
    frozen_upto: Option<u64>,
    pub topic_filter: String,
    pub text_filter: String,
    pub retained_only: bool,
    pub show_events: bool,
    pub typing: Option<Field>,
    /// Index into the shown entries.
    pub selection: usize,
    /// First shown entry of the window.
    pub top: usize,
    pub viewport: usize,
    pub expanded: BTreeSet<u64>,
    details: HashMap<u64, Vec<DetailLine>>,
    /// Ids of the entries that pass the filters, oldest first.
    visible: Vec<u64>,
    processed: Option<u64>,
    applied: Option<(String, String, bool, bool)>,
}

impl Default for StreamView {
    fn default() -> Self {
        Self {
            follow: true,
            paused: false,
            frozen_upto: None,
            topic_filter: String::new(),
            text_filter: String::new(),
            retained_only: false,
            show_events: true,
            typing: None,
            selection: 0,
            top: 0,
            viewport: 10,
            expanded: BTreeSet::new(),
            details: HashMap::new(),
            visible: Vec::new(),
            processed: None,
            applied: None,
        }
    }
}

impl StreamView {
    pub fn reset(&mut self) {
        *self = Self {
            topic_filter: std::mem::take(&mut self.topic_filter),
            text_filter: std::mem::take(&mut self.text_filter),
            retained_only: self.retained_only,
            show_events: self.show_events,
            ..Self::default()
        };
    }

    pub fn filters_active(&self) -> bool {
        !self.topic_filter.trim().is_empty() || !self.text_filter.is_empty() || self.retained_only
    }

    /// The entries the list shows: frozen at the moment of the pause while paused.
    pub fn shown(&self) -> &[u64] {
        let end = match self.frozen_upto {
            Some(limit) => self.visible.partition_point(|id| *id <= limit),
            None => self.visible.len(),
        };
        &self.visible[..end]
    }

    /// Entries that arrived while paused and match the filters.
    pub fn waiting(&self) -> usize {
        self.visible.len() - self.shown().len()
    }

    pub fn selected_id(&self) -> Option<u64> {
        self.shown().get(self.selection).copied()
    }

    fn matches(&self, entry: &StreamEntry) -> bool {
        match &entry.kind {
            StreamKind::Event { .. } => self.show_events && !self.filters_active(),
            StreamKind::Message(message) => {
                let topic = self.topic_filter.trim();
                (topic.is_empty() || match_positions(topic, &message.topic).is_some())
                    && content_matches(
                        &message.payload,
                        message.properties.as_deref(),
                        &self.text_filter,
                    )
                    && (!self.retained_only || message.retain)
            }
        }
    }

    /// Brings the visible ids in line with the log; only new entries are evaluated.
    pub fn refresh(&mut self, log: &StreamLog) {
        let key = (
            self.topic_filter.trim().to_owned(),
            self.text_filter.clone(),
            self.retained_only,
            self.show_events,
        );
        if self.applied.as_ref() != Some(&key) {
            self.visible.clear();
            self.processed = None;
            self.applied = Some(key);
            self.follow = true;
        }
        let first = log.first_id();
        let removed = first.map_or(self.visible.len(), |first| {
            self.visible.partition_point(|id| *id < first)
        });
        if removed > 0 {
            self.visible.drain(..removed);
            self.selection = self.selection.saturating_sub(removed);
            self.top = self.top.saturating_sub(removed);
        }
        match first {
            Some(first) => {
                self.expanded = self.expanded.split_off(&first);
                self.details.retain(|id, _| *id >= first);
            }
            None => {
                self.expanded.clear();
                self.details.clear();
            }
        }
        let fresh: Vec<u64> = log
            .since(self.processed)
            .filter(|entry| self.matches(entry))
            .map(|entry| entry.id)
            .collect();
        self.visible.extend(fresh);
        if let Some(last) = log.last_id() {
            self.processed = Some(last);
        }
        let shown = self.shown().len();
        if self.follow || self.selection >= shown {
            self.selection = shown.saturating_sub(1);
        }
    }

    pub fn details(&mut self, log: &StreamLog, id: u64) -> &[DetailLine] {
        self.details.entry(id).or_insert_with(|| match log.get(id) {
            Some(StreamEntry {
                kind: StreamKind::Message(message),
                ..
            }) => detail_lines(message),
            _ => Vec::new(),
        })
    }

    pub fn height(&mut self, log: &StreamLog, id: u64) -> usize {
        if self.expanded.contains(&id) {
            1 + self.details(log, id).len()
        } else {
            1
        }
    }

    /// Scrolls the window just far enough to show the selected entry completely.
    pub fn fit_window(&mut self, log: &StreamLog, height: usize) {
        let shown = self.shown().len();
        if shown == 0 {
            self.top = 0;
            return;
        }
        let selection = self.selection.min(shown - 1);
        self.top = self.top.min(selection);
        let mut used = 0;
        for index in self.top..=selection {
            let id = self.shown()[index];
            used += self.height(log, id);
            if used > height {
                break;
            }
        }
        if used <= height {
            return;
        }
        let mut used = 0;
        let mut top = selection;
        for index in (0..=selection).rev() {
            let id = self.shown()[index];
            let entry = self.height(log, id);
            if index != selection && used + entry > height {
                break;
            }
            used += entry;
            top = index;
        }
        self.top = top;
    }

    fn move_selection(&mut self, delta: isize) {
        let last = self.shown().len().saturating_sub(1);
        self.selection = self.selection.saturating_add_signed(delta).min(last);
        self.follow = self.selection == last;
    }

    fn edit_field(&mut self, field: Field, code: KeyCode) {
        let text = match field {
            Field::Topic => &mut self.topic_filter,
            Field::Text => &mut self.text_filter,
        };
        match code {
            KeyCode::Esc => {
                text.clear();
                self.typing = None;
            }
            KeyCode::Enter => self.typing = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(character) => text.push(character),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            _ => {}
        }
    }

    /// Expands or collapses the selected message; `None` toggles. Connection events have no details.
    fn expand_selected(&mut self, log: &StreamLog, wanted: Option<bool>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        if !matches!(
            log.get(id).map(|entry| &entry.kind),
            Some(StreamKind::Message(_))
        ) {
            return;
        }
        if wanted.unwrap_or(!self.expanded.contains(&id)) {
            self.expanded.insert(id);
        } else {
            self.expanded.remove(&id);
        }
    }

    fn toggle_pause(&mut self, log: &StreamLog) {
        self.paused = !self.paused;
        self.frozen_upto = self.paused.then(|| log.last_id()).flatten();
        if !self.paused {
            self.follow = true;
        }
    }
}

/// The text of the message as a string, for copying.
fn payload_text(message: &StreamMessage) -> String {
    String::from_utf8_lossy(&message.payload).into_owned()
}

/// The one-line preview of a message in the list.
pub fn list_preview(message: &StreamMessage) -> String {
    payload_preview(&message.payload)
        .chars()
        .take(PREVIEW_CHARS)
        .collect()
}

impl UiState {
    pub(super) fn open_stream(&mut self) {
        self.view = View::Stream;
        self.stream_view.refresh(&self.stream);
    }

    /// Keeps the list in line with the log; called before every render of the stream view.
    pub(super) fn refresh_stream(&mut self) {
        self.stream_view.refresh(&self.stream);
    }

    pub(super) fn handle_stream_key(&mut self, code: KeyCode) -> UiAction {
        let page = self.stream_view.viewport.saturating_sub(1).max(1) as isize;
        let view = &mut self.stream_view;
        if let Some(field) = view.typing {
            view.edit_field(field, code);
            view.refresh(&self.stream);
            return UiAction::None;
        }
        match code {
            KeyCode::Esc if view.filters_active() => {
                view.topic_filter.clear();
                view.text_filter.clear();
                view.retained_only = false;
            }
            KeyCode::Esc | KeyCode::Char('S') => self.view = View::Main,
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Up | KeyCode::Char('k') => view.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => view.move_selection(1),
            KeyCode::PageUp => view.move_selection(-page),
            KeyCode::PageDown => view.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => view.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => view.move_selection(isize::MAX / 2),
            KeyCode::Char(' ') => view.toggle_pause(&self.stream),
            KeyCode::Char('/') => view.typing = Some(Field::Topic),
            KeyCode::Char('f') => view.typing = Some(Field::Text),
            KeyCode::Char('R') => view.retained_only = !view.retained_only,
            KeyCode::Char('C') => {
                let cleared = self.stream.len();
                self.stream.clear();
                view.reset();
                self.notify(format!("Stream cleared ({cleared} entries)"), false);
            }
            KeyCode::Char('e') => view.show_events = !view.show_events,
            KeyCode::Char('+') => view.expand_selected(&self.stream, Some(true)),
            KeyCode::Char('-') => view.expand_selected(&self.stream, Some(false)),
            KeyCode::Enter => view.expand_selected(&self.stream, None),
            KeyCode::Char('s' | 'c' | 't') => {
                let entry = view.selected_id().and_then(|id| self.stream.get(id));
                let Some(StreamKind::Message(message)) = entry.map(|entry| &entry.kind) else {
                    self.notify("Select a message first", true);
                    return UiAction::None;
                };
                let (topic, payload) = (message.topic.clone(), payload_text(message));
                match code {
                    KeyCode::Char('s') => {
                        self.main.reveal(&topic);
                        self.main.focus = Panel::Topics;
                        self.main.ensure_rows(&self.topics);
                        self.view = View::Main;
                    }
                    KeyCode::Char('c') => self.copy(Some(payload), "payload"),
                    _ => self.copy(Some(topic), "topic"),
                }
            }
            _ => {}
        }
        self.stream_view.refresh(&self.stream);
        UiAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mate::topics::MessageProperties;
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    fn message(payload: &[u8]) -> StreamMessage {
        StreamMessage {
            topic: "plc/t".to_owned(),
            payload: payload.to_vec().into(),
            qos: 1,
            retain: true,
            dup: false,
            properties: None,
            since_previous: Some(Duration::from_millis(250)),
            broker: None,
        }
    }

    fn texts(lines: &[DetailLine]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| match line {
                DetailLine::Info(text)
                | DetailLine::Property(text)
                | DetailLine::Json(text)
                | DetailLine::Text(text)
                | DetailLine::Note(text) => text.as_str(),
            })
            .collect()
    }

    #[test]
    fn details_show_facts_properties_and_a_formatted_payload() {
        let mut with_properties = message(br#"{"a":1}"#);
        with_properties.properties = Some(Arc::new(MessageProperties {
            content_type: Some("application/json".to_owned()),
            user_properties: vec![("origin".to_owned(), "plc-7".to_owned())],
            ..MessageProperties::default()
        }));
        let lines = detail_lines(&with_properties);
        let text = texts(&lines);
        assert!(
            text[0].contains("QoS 1") && text[0].contains("retained") && text[0].contains("250 ms")
        );
        assert!(
            text.contains(&"content type: application/json") && text.contains(&"origin = plc-7")
        );
        assert!(matches!(lines.last(), Some(DetailLine::Json(line)) if line == "}"));
        assert!(text.contains(&"  \"a\": 1"));
    }

    #[test]
    fn details_handle_text_binary_and_empty_payloads_and_cap_long_ones() {
        assert_eq!(texts(&detail_lines(&message(b"")))[1], "(empty payload)");
        assert_eq!(texts(&detail_lines(&message(b"RUNNING")))[1], "RUNNING");
        let binary = detail_lines(&message(&[0xff, 0x00, 0x41]));
        assert!(texts(&binary)[1].starts_with("0000  ff 00 41"));
        let long = "line\n".repeat(100);
        let lines = detail_lines(&message(long.as_bytes()));
        assert_eq!(lines.len(), 1 + MAX_DETAIL_LINES + 1);
        assert!(
            matches!(lines.last(), Some(DetailLine::Note(note)) if note.contains("60 more lines"))
        );
    }

    fn log_of(topics: &[&str]) -> StreamLog {
        let mut log = StreamLog::default();
        for topic in topics {
            let mut entry = message(b"x");
            entry.topic = (*topic).to_owned();
            entry.retain = false;
            log.push_message(SystemTime::now(), entry);
        }
        log
    }

    #[test]
    fn refresh_only_evaluates_new_entries_and_follows_the_newest() {
        let mut log = log_of(&["a/1", "b/1"]);
        let mut view = StreamView::default();
        view.refresh(&log);
        assert_eq!((view.shown().len(), view.selection), (2, 1));
        log.push_message(SystemTime::now(), message(b"y"));
        view.refresh(&log);
        assert_eq!((view.shown().len(), view.selection), (3, 2));

        view.topic_filter = "a/".to_owned();
        view.refresh(&log);
        assert_eq!(
            view.shown().len(),
            1,
            "the filter is applied to the whole log"
        );
    }

    #[test]
    fn pausing_freezes_the_list_and_counts_waiting_entries() {
        let mut log = log_of(&["a"]);
        let mut view = StreamView::default();
        view.refresh(&log);
        view.toggle_pause(&log);
        log.push_message(SystemTime::now(), message(b"y"));
        view.refresh(&log);
        assert_eq!((view.shown().len(), view.waiting()), (1, 1));
        view.toggle_pause(&log);
        assert_eq!((view.shown().len(), view.waiting()), (2, 0));
    }

    #[test]
    fn evicted_entries_leave_the_list_and_the_selection_follows_the_shift() {
        let mut log = StreamLog::with_limits(3, usize::MAX);
        let mut view = StreamView::default();
        for index in 0..3 {
            log.push_message(SystemTime::now(), message(format!("{index}").as_bytes()));
        }
        view.refresh(&log);
        view.follow = false;
        view.selection = 2;
        view.expanded.insert(0);
        log.push_message(SystemTime::now(), message(b"3"));
        view.refresh(&log);
        assert_eq!((view.shown().len(), view.selection), (3, 1));
        assert!(
            view.expanded.is_empty(),
            "details of evicted entries are forgotten"
        );
    }

    #[test]
    fn the_window_scrolls_only_as_far_as_the_selection_needs() {
        let mut log = log_of(&["a", "b", "c", "d", "e", "f"]);
        let mut view = StreamView::default();
        view.refresh(&log);
        view.follow = false;
        view.selection = 0;
        view.fit_window(&log, 3);
        assert_eq!(view.top, 0);
        view.selection = 2;
        view.fit_window(&log, 3);
        assert_eq!(view.top, 0, "still fits");
        view.selection = 3;
        view.fit_window(&log, 3);
        assert_eq!(view.top, 1);
        view.selection = 1;
        view.fit_window(&log, 3);
        assert_eq!(view.top, 1, "no scrolling while the selection is inside");

        let id = view.shown()[3];
        view.expanded.insert(id);
        let extra = view.height(&log, id) - 1;
        assert!(extra >= 2);
        view.selection = 3;
        view.fit_window(&log, 3);
        assert!(view.top >= 2, "the expanded entry needs room");
        log.clear();
    }
}
