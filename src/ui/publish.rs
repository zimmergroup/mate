//! The publish dialog: edit a message, review it, then hand a validated request to the event loop.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mate::{
    mqtt::{PublishRequest, validate_publish},
    ui_state::{PublishHistoryEntry, remember_publish},
};
use std::time::{SystemTime, UNIX_EPOCH};

use mate::topics::Message;

use super::{UiAction, UiState, View, layout::Panel};
use mate::mqtt::ConnectionState;

pub const FIELD_COUNT: usize = 4;
pub const FIELD_TOPIC: usize = 0;
pub const FIELD_PAYLOAD: usize = 1;
pub const FIELD_QOS: usize = 2;
pub const FIELD_RETAIN: usize = 3;
/// The list of earlier messages below the fields; only reachable when there are any.
pub const FIELD_RECENT: usize = 4;

pub(super) fn byte_at(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

pub(super) fn insert_at(text: &mut String, cursor: &mut usize, inserted: &str) {
    text.insert_str(byte_at(text, *cursor), inserted);
    *cursor += inserted.chars().count();
}

pub(super) fn backspace_at(text: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        text.remove(byte_at(text, *cursor - 1));
        *cursor -= 1;
    }
}

pub(super) fn delete_at(text: &mut String, cursor: usize) {
    if cursor < text.chars().count() {
        text.remove(byte_at(text, cursor));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStage {
    Edit,
    Review,
}

#[derive(Debug, Clone)]
pub struct PublishEditor {
    pub topic: String,
    /// Cursor in the topic as a character index.
    pub topic_cursor: usize,
    pub payload: String,
    /// Cursor in the payload as a character index.
    pub cursor: usize,
    pub qos: u8,
    pub retain: bool,
    pub field: usize,
    pub stage: PublishStage,
    /// Lives in the publish panel instead of a dialog.
    pub docked: bool,
    pub history_selection: usize,
    /// Path being typed after Ctrl+O.
    pub file_prompt: Option<String>,
    pub error: Option<String>,
    /// ID of the broker the message goes to.
    pub broker_id: String,
}

impl PublishEditor {
    pub fn insert(&mut self, text: &str) {
        insert_at(&mut self.payload, &mut self.cursor, text);
    }

    pub fn backspace(&mut self) {
        backspace_at(&mut self.payload, &mut self.cursor);
    }

    pub fn delete(&mut self) {
        delete_at(&mut self.payload, self.cursor);
    }

    pub fn insert_topic(&mut self, text: &str) {
        insert_at(&mut self.topic, &mut self.topic_cursor, text);
    }

    pub fn set_topic(&mut self, topic: &str) {
        self.topic = topic.to_owned();
        self.topic_cursor = topic.chars().count();
    }

    /// Replaces the payload and puts the cursor behind it.
    pub fn set_payload(&mut self, payload: &str) {
        self.payload = payload.to_owned();
        self.cursor = payload.chars().count();
    }

    /// Zero-based line and column of the cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let before = &self.payload[..byte_at(&self.payload, self.cursor)];
        let line = before.matches('\n').count();
        let col = before
            .rsplit('\n')
            .next()
            .map_or(0, |last| last.chars().count());
        (line, col)
    }

    pub fn line_count(&self) -> usize {
        self.payload.matches('\n').count() + 1
    }

    /// Moves between lines; false at the first or last line, where the caller leaves the field.
    pub fn move_vertical(&mut self, delta: isize) -> bool {
        let lengths: Vec<usize> = self
            .payload
            .split('\n')
            .map(|l| l.chars().count())
            .collect();
        let (line, col) = self.line_col();
        let Some(target) = line
            .checked_add_signed(delta)
            .filter(|t| *t < lengths.len())
        else {
            return false;
        };
        let start: usize = lengths[..target].iter().map(|length| length + 1).sum();
        self.cursor = start + col.min(lengths[target]);
        true
    }

    pub fn line_edge(&mut self, end: bool) {
        let lengths: Vec<usize> = self
            .payload
            .split('\n')
            .map(|l| l.chars().count())
            .collect();
        let (line, _) = self.line_col();
        let start: usize = lengths[..line].iter().map(|length| length + 1).sum();
        self.cursor = if end { start + lengths[line] } else { start };
    }

    /// Switches a JSON payload between the indented form and one line; anything else is left alone.
    pub fn toggle_json_format(&mut self) -> Result<(), String> {
        let value: serde_json::Value = serde_json::from_str(&self.payload)
            .map_err(|error| format!("Not valid JSON: {error}"))?;
        let pretty = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
        let text = if self.payload == pretty {
            serde_json::to_string(&value).map_err(|error| error.to_string())?
        } else {
            pretty
        };
        self.set_payload(&text);
        Ok(())
    }

    pub fn request(&self) -> PublishRequest {
        PublishRequest {
            topic: self.topic.trim().to_owned(),
            payload: self.payload.clone().into_bytes(),
            qos: self.qos,
            retain: self.retain,
            user_properties: Vec::new(),
        }
    }

    /// Mistakes that sending without a review must not skip.
    pub fn needs_review(&self) -> Option<&'static str> {
        if self.retain && self.payload.is_empty() {
            Some("an empty retained payload deletes the retained message")
        } else if looks_like_json(&self.payload)
            && serde_json::from_str::<serde_json::Value>(&self.payload).is_err()
        {
            Some("the payload looks like JSON but is not valid JSON")
        } else {
            None
        }
    }

    /// Retained messages outlive this session; an empty one removes the existing retained message.
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if self.retain && self.payload.is_empty() {
            warnings.push("An empty retained payload DELETES the retained message on this topic.");
        } else if self.retain {
            warnings.push(
                "Retained: the broker keeps this message and sends it to every future subscriber.",
            );
        } else if self.payload.is_empty() {
            warnings.push("The payload is empty.");
        }
        if looks_like_json(&self.payload)
            && serde_json::from_str::<serde_json::Value>(&self.payload).is_err()
        {
            warnings.push("The payload looks like JSON but is not valid JSON.");
        }
        warnings
    }
}

/// Reads a UTF-8 text file for the payload; one trailing line break is dropped.
pub fn read_payload_file(input: &str) -> Result<String, String> {
    use std::io::Read;
    let max = mate::mqtt::MAX_PUBLISH_PAYLOAD;
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter the path of a text file".to_owned());
    }
    let path = match input.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .map_or_else(|| input.into(), |dirs| dirs.home_dir().join(rest)),
        None => std::path::PathBuf::from(input),
    };
    let unreadable = |error: std::io::Error| format!("Cannot read {input}: {error}");
    if !std::fs::metadata(&path).map_err(unreadable)?.is_file() {
        return Err(format!("{input} is not a regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(max as u64 + 1).read_to_end(&mut bytes))
        .map_err(unreadable)?;
    if bytes.len() > max {
        return Err(format!("{input} is larger than {max} bytes"));
    }
    let mut text = String::from_utf8(bytes)
        .map_err(|_| format!("{input} is not UTF-8 text; binary payloads cannot be loaded"))?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

pub(super) fn looks_like_json(payload: &str) -> bool {
    matches!(payload.trim_start().chars().next(), Some('{' | '['))
}

impl UiState {
    /// Connected brokers that allow publishing, in connection order.
    pub(super) fn publishable_brokers(&self) -> impl Iterator<Item = &super::ConnectionInfo> {
        self.connections.iter().filter(|connection| {
            connection.state == ConnectionState::Connected && connection.allow_publish
        })
    }

    /// Why publishing is impossible right now, if it is.
    pub(super) fn publish_blocker(&self) -> Option<String> {
        if self.publishable_brokers().next().is_some() {
            return None;
        }
        if self.connections.is_empty() {
            return Some("Connect to a broker first (o)".to_owned());
        }
        let connected: Vec<_> = self
            .connections
            .iter()
            .filter(|connection| connection.state == ConnectionState::Connected)
            .collect();
        match connected.as_slice() {
            [] => Some("Not connected; publishing needs an established connection".to_owned()),
            [connection] => Some(format!(
                "Publishing is off for {}: press b, select it and press w to allow it",
                connection.broker_name
            )),
            _ => Some(
                "Publishing is off for every connected broker: press b, select one and press w to allow it"
                    .to_owned(),
            ),
        }
    }

    /// The broker a new message goes to: the one that delivered the selected topic, else the first.
    pub(super) fn default_publish_broker(&self) -> Option<String> {
        let delivered_by = self
            .main
            .selected
            .as_deref()
            .and_then(|topic| self.topics.latest_message(topic))
            .and_then(|message| message.broker.as_deref());
        if let Some(broker) = delivered_by
            && self
                .publishable_brokers()
                .any(|connection| connection.broker_id == broker)
        {
            return Some(broker.to_owned());
        }
        self.publishable_brokers()
            .next()
            .or_else(|| self.connections.first())
            .map(|connection| connection.broker_id.clone())
    }

    /// Ctrl+B: sends to the next connected broker that allows publishing.
    fn cycle_publish_broker(&mut self) {
        let ids: Vec<String> = self
            .publishable_brokers()
            .map(|connection| connection.broker_id.clone())
            .collect();
        let Some(editor) = self.publish_editor.as_mut() else {
            return;
        };
        if ids.len() < 2 {
            return;
        }
        let next = ids
            .iter()
            .position(|id| *id == editor.broker_id)
            .map_or(0, |index| (index + 1) % ids.len());
        editor.broker_id = ids[next].clone();
        editor.history_selection = 0;
    }

    /// The message a new editor is filled from: the one shown in the inspector, or under the history cursor.
    fn publish_template(&self) -> Option<&Message> {
        let topic = self.main.selected.as_deref()?;
        match self.main.focus {
            Panel::Inspector => self.displayed_message(),
            Panel::History => self
                .history_cursor_seq()
                .and_then(|seq| self.topics.find_message(topic, seq)),
            Panel::Topics | Panel::Publish => None,
        }
    }

    fn new_publish_editor(&self, docked: bool) -> PublishEditor {
        let mut editor = self.blank_publish_editor(docked);
        if let Some(message) = self.publish_template()
            && let Ok(text) = std::str::from_utf8(&message.payload)
        {
            editor.set_payload(text);
            // At the start, where a header with a sequence number or timestamp usually is.
            editor.cursor = 0;
            editor.field = FIELD_PAYLOAD;
            editor.qos = message.qos;
            editor.retain = message.retain;
            if let Some(broker) = message.broker.as_deref()
                && self
                    .publishable_brokers()
                    .any(|connection| connection.broker_id == broker)
            {
                editor.broker_id = broker.to_owned();
            }
        }
        editor
    }

    fn blank_publish_editor(&self, docked: bool) -> PublishEditor {
        let topic = match (
            self.main.selected.as_deref(),
            self.main.rows.get(self.main.selected_index),
        ) {
            (Some(path), Some(row)) if row.has_value => path.to_owned(),
            (Some(path), _) => format!("{path}/"),
            _ => String::new(),
        };
        PublishEditor {
            topic_cursor: topic.chars().count(),
            topic,
            payload: String::new(),
            cursor: 0,
            qos: 0,
            retain: false,
            field: FIELD_TOPIC,
            stage: PublishStage::Edit,
            docked,
            history_selection: 0,
            file_prompt: None,
            error: None,
            broker_id: self.default_publish_broker().unwrap_or_default(),
        }
    }

    /// `p`: the dialog, or the panel when it is shown.
    pub(super) fn open_publish_editor(&mut self) {
        if let Some(reason) = self.publish_blocker() {
            self.notify(reason, true);
            return;
        }
        if self.main.layout.show_publish {
            // A message picked in the inspector or history replaces the panel's draft.
            if self.publish_template().is_some() {
                self.publish_editor = Some(self.new_publish_editor(true));
            }
            self.main.focus = Panel::Publish;
            return;
        }
        if self
            .publish_template()
            .is_some_and(|message| std::str::from_utf8(&message.payload).is_err())
        {
            self.notify("The payload is binary and not copied into the editor", true);
        }
        self.publish_editor = Some(self.new_publish_editor(false));
    }

    /// `P`: shows the panel with the parked draft, or hides it and parks the draft.
    pub(super) fn toggle_publish_panel(&mut self) {
        let layout = &mut self.main.layout;
        if layout.show_publish {
            layout.show_publish = false;
            if layout.maximized == Some(Panel::Publish) {
                layout.maximized = None;
            }
            if self.main.focus == Panel::Publish {
                self.main.focus = Panel::Topics;
            }
            self.publish_draft = self.publish_editor.take().filter(|editor| editor.docked);
            return;
        }
        if let Some(reason) = self.publish_blocker() {
            self.notify(reason, true);
            return;
        }
        self.main.layout.show_publish = true;
        self.publish_editor = Some(
            self.publish_draft
                .take()
                .unwrap_or_else(|| self.new_publish_editor(true)),
        );
        self.main.focus = Panel::Publish;
    }

    /// A dialog, or the focused panel, takes the keystrokes.
    pub(super) fn publish_typing(&self) -> bool {
        self.publish_editor.as_ref().is_some_and(|editor| {
            !editor.docked
                || (self.view == View::Main
                    && self.main.layout.show_publish
                    && self.main.focus == Panel::Publish)
        })
    }

    /// The dialog is open, or the panel's message is waiting for the review.
    pub(super) fn publish_overlay(&self) -> bool {
        self.publish_editor
            .as_ref()
            .is_some_and(|editor| !editor.docked || editor.stage == PublishStage::Review)
    }

    /// Keys for the dialog or the focused panel; the panel keeps its draft when a message was sent.
    pub(super) fn dispatch_publish_key(&mut self, key: KeyEvent) -> UiAction {
        let docked = self
            .publish_editor
            .as_ref()
            .is_some_and(|editor| editor.docked);
        if docked
            && key.code == KeyCode::Esc
            && self.publish_editor.as_ref().is_some_and(|editor| {
                editor.stage == PublishStage::Edit && editor.file_prompt.is_none()
            })
        {
            self.main.focus = Panel::Topics;
            return UiAction::None;
        }
        let backup = docked.then(|| self.publish_editor.clone()).flatten();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let action = if ctrl && key.code == KeyCode::Char('s') {
            self.publish_send_now()
        } else if ctrl && key.code == KeyCode::Char('b') {
            self.cycle_publish_broker();
            UiAction::None
        } else if self.publish_payload_shortcut(key) {
            UiAction::None
        } else if ctrl
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char(_))
        {
            // An unassigned Ctrl+letter must not type the letter; Ctrl+Alt is AltGr on some systems.
            UiAction::None
        } else {
            self.handle_publish_key(key.code)
        };
        if self.publish_editor.is_none()
            && let Some(mut editor) = backup
        {
            editor.stage = PublishStage::Edit;
            editor.error = None;
            self.publish_editor = Some(editor);
        }
        action
    }

    /// Messages published to `broker_id`, newest first.
    pub(super) fn publish_history_of(&self, broker_id: &str) -> &[PublishHistoryEntry] {
        self.publish_history
            .get(broker_id)
            .map_or(&[], Vec::as_slice)
    }

    /// Messages published to the broker a new message would go to.
    #[cfg(test)]
    pub(super) fn publish_history(&self) -> &[PublishHistoryEntry] {
        let broker = self.default_publish_broker().unwrap_or_default();
        self.publish_history_of(&broker)
    }

    fn remember_published(&mut self, broker_id: &str, request: &PublishRequest) {
        let broker_id = broker_id.to_owned();
        let sent_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        remember_publish(
            self.publish_history.entry(broker_id).or_default(),
            PublishHistoryEntry {
                topic: request.topic.clone(),
                payload: String::from_utf8_lossy(&request.payload).into_owned(),
                qos: request.qos,
                retain: request.retain,
                sent_at,
            },
        );
    }

    /// Ctrl+S: sends without the review step, except for mistakes the review exists for.
    pub(super) fn publish_send_now(&mut self) -> UiAction {
        let Some(mut editor) = self.publish_editor.take() else {
            return UiAction::None;
        };
        if editor.file_prompt.is_some() {
            self.publish_editor = Some(editor);
            return UiAction::None;
        }
        if editor.stage == PublishStage::Review {
            return UiAction::Publish(editor.broker_id.clone(), editor.request());
        }
        editor.error = None;
        if let Err(error) = validate_publish(&editor.request()) {
            editor.error = Some(format!("{error:#}"));
        } else if let Some(reason) = editor.needs_review() {
            editor.stage = PublishStage::Review;
            self.notify(format!("Please review first: {reason}"), true);
        } else {
            return UiAction::Publish(editor.broker_id.clone(), editor.request());
        }
        self.publish_editor = Some(editor);
        UiAction::None
    }

    /// Payload shortcuts that need a modifier key; returns false for any other key.
    pub(super) fn publish_payload_shortcut(&mut self, key: KeyEvent) -> bool {
        let Some(editor) = self.publish_editor.as_mut() else {
            return false;
        };
        if editor.stage != PublishStage::Edit
            || editor.field != FIELD_PAYLOAD
            || editor.file_prompt.is_some()
        {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        editor.error = None;
        match key.code {
            KeyCode::Char('j') if ctrl => editor.insert("\n"),
            KeyCode::Enter if alt => editor.insert("\n"),
            KeyCode::Char('f') if ctrl => {
                if let Err(error) = editor.toggle_json_format() {
                    editor.error = Some(error);
                }
            }
            KeyCode::Char('u') if ctrl => editor.set_payload(""),
            KeyCode::Char('o') if ctrl => editor.file_prompt = Some(String::new()),
            _ => return false,
        }
        true
    }

    pub(super) fn handle_publish_key(&mut self, code: KeyCode) -> UiAction {
        let Some(mut editor) = self.publish_editor.take() else {
            return UiAction::None;
        };
        if editor.stage == PublishStage::Review {
            return match code {
                KeyCode::Char('y') => UiAction::Publish(editor.broker_id.clone(), editor.request()),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Backspace => {
                    editor.stage = PublishStage::Edit;
                    self.publish_editor = Some(editor);
                    UiAction::None
                }
                _ => {
                    self.publish_editor = Some(editor);
                    UiAction::None
                }
            };
        }
        editor.error = None;
        if let Some(path) = editor.file_prompt.as_mut() {
            match code {
                KeyCode::Esc => editor.file_prompt = None,
                KeyCode::Enter => {
                    let path = std::mem::take(path);
                    editor.file_prompt = None;
                    match read_payload_file(&path) {
                        Ok(text) => editor.set_payload(&text),
                        Err(error) => editor.error = Some(error),
                    }
                }
                KeyCode::Backspace => {
                    path.pop();
                }
                KeyCode::Char(character) => path.push(character),
                _ => {}
            }
            self.publish_editor = Some(editor);
            return UiAction::None;
        }
        let recent = self.publish_history_of(&editor.broker_id).to_vec();
        let fields = if recent.is_empty() {
            FIELD_COUNT
        } else {
            FIELD_COUNT + 1
        };
        editor.history_selection = editor.history_selection.min(recent.len().saturating_sub(1));
        match code {
            KeyCode::Esc => return UiAction::None,
            KeyCode::Tab => editor.field = (editor.field + 1) % fields,
            KeyCode::BackTab => editor.field = (editor.field + fields - 1) % fields,
            KeyCode::Down if editor.field == FIELD_RECENT => {
                if editor.history_selection + 1 < recent.len() {
                    editor.history_selection += 1;
                } else {
                    editor.field = FIELD_TOPIC;
                }
            }
            KeyCode::Up if editor.field == FIELD_RECENT => {
                if editor.history_selection > 0 {
                    editor.history_selection -= 1;
                } else {
                    editor.field = FIELD_RETAIN;
                }
            }
            KeyCode::Down if editor.field == FIELD_PAYLOAD && editor.move_vertical(1) => {}
            KeyCode::Up if editor.field == FIELD_PAYLOAD && editor.move_vertical(-1) => {}
            KeyCode::Down => {
                editor.field = (editor.field + 1) % fields;
                editor.history_selection = 0;
            }
            KeyCode::Up => {
                editor.field = (editor.field + fields - 1) % fields;
                editor.history_selection = recent.len().saturating_sub(1);
            }
            KeyCode::Enter if editor.field == FIELD_RECENT => {
                if let Some(entry) = recent.get(editor.history_selection) {
                    editor.set_topic(&entry.topic);
                    editor.set_payload(&entry.payload);
                    editor.qos = entry.qos;
                    editor.retain = entry.retain;
                }
                editor.field = FIELD_PAYLOAD;
            }
            KeyCode::Char('d') | KeyCode::Delete if editor.field == FIELD_RECENT => {
                let selected = editor.history_selection;
                if let Some(list) = self.publish_history.get_mut(&editor.broker_id)
                    && selected < list.len()
                {
                    list.remove(selected);
                    if list.is_empty() {
                        editor.field = FIELD_PAYLOAD;
                    }
                }
            }
            KeyCode::Enter if editor.field + 1 < FIELD_COUNT => editor.field += 1,
            KeyCode::Enter => match validate_publish(&editor.request()) {
                Ok(()) => editor.stage = PublishStage::Review,
                Err(error) => editor.error = Some(format!("{error:#}")),
            },
            KeyCode::Backspace => match editor.field {
                FIELD_TOPIC => backspace_at(&mut editor.topic, &mut editor.topic_cursor),
                FIELD_PAYLOAD => editor.backspace(),
                _ => {}
            },
            KeyCode::Delete if editor.field == FIELD_TOPIC => {
                delete_at(&mut editor.topic, editor.topic_cursor)
            }
            KeyCode::Left if editor.field == FIELD_TOPIC => {
                editor.topic_cursor = editor.topic_cursor.saturating_sub(1)
            }
            KeyCode::Right if editor.field == FIELD_TOPIC => {
                editor.topic_cursor = (editor.topic_cursor + 1).min(editor.topic.chars().count())
            }
            KeyCode::Home if editor.field == FIELD_TOPIC => editor.topic_cursor = 0,
            KeyCode::End if editor.field == FIELD_TOPIC => {
                editor.topic_cursor = editor.topic.chars().count()
            }
            KeyCode::Delete if editor.field == FIELD_PAYLOAD => editor.delete(),
            KeyCode::Left if editor.field == FIELD_PAYLOAD => {
                editor.cursor = editor.cursor.saturating_sub(1)
            }
            KeyCode::Right if editor.field == FIELD_PAYLOAD => {
                editor.cursor = (editor.cursor + 1).min(editor.payload.chars().count())
            }
            KeyCode::Home if editor.field == FIELD_PAYLOAD => editor.line_edge(false),
            KeyCode::End if editor.field == FIELD_PAYLOAD => editor.line_edge(true),
            KeyCode::Left if editor.field == FIELD_QOS => editor.qos = editor.qos.saturating_sub(1),
            KeyCode::Right if editor.field == FIELD_QOS => editor.qos = (editor.qos + 1).min(2),
            KeyCode::Left | KeyCode::Right if editor.field == FIELD_RETAIN => {
                editor.retain = !editor.retain
            }
            KeyCode::Char(character) => match editor.field {
                FIELD_TOPIC => editor.insert_topic(character.encode_utf8(&mut [0; 4])),
                FIELD_PAYLOAD => editor.insert(character.encode_utf8(&mut [0; 4])),
                FIELD_QOS => {
                    if let Some(qos) = character.to_digit(10).filter(|qos| *qos <= 2) {
                        editor.qos = qos as u8;
                    }
                }
                FIELD_RETAIN => match character {
                    ' ' => editor.retain = !editor.retain,
                    'y' => editor.retain = true,
                    'n' => editor.retain = false,
                    _ => {}
                },
                _ => {}
            },
            _ => {}
        }
        self.publish_editor = Some(editor);
        UiAction::None
    }

    /// Reports the outcome of a publish attempt made by the event loop.
    pub fn report_publish(
        &mut self,
        broker_id: &str,
        request: &PublishRequest,
        result: Result<(), String>,
    ) {
        self.dirty = true;
        if result.is_ok() {
            self.remember_published(broker_id, request);
            self.persist_ui_settings();
        }
        match result {
            Ok(()) if request.retain && request.payload.is_empty() => self.notify(
                format!("Cleared the retained message on {}", request.topic),
                false,
            ),
            Ok(()) => {
                let broker = if self.connections.len() > 1 {
                    self.connection_of(broker_id)
                        .map(|connection| format!(" on {}", connection.broker_name))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                self.notify(
                    format!(
                        "Published {} bytes to {}{broker} (QoS {}{})",
                        request.payload.len(),
                        request.topic,
                        request.qos,
                        if request.retain { ", retained" } else { "" }
                    ),
                    false,
                )
            }
            Err(error) => self.notify(format!("Not published: {error}"), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(topic: &str, payload: &str, retain: bool) -> PublishEditor {
        PublishEditor {
            topic: topic.to_owned(),
            topic_cursor: topic.chars().count(),
            payload: payload.to_owned(),
            cursor: payload.chars().count(),
            qos: 1,
            retain,
            field: 0,
            stage: PublishStage::Edit,
            docked: false,
            history_selection: 0,
            file_prompt: None,
            error: None,
            broker_id: String::new(),
        }
    }

    #[test]
    fn warnings_explain_retained_empty_and_broken_json_payloads() {
        assert!(editor("a", "ok", false).warnings().is_empty());
        assert!(editor("a", "ok", true).warnings()[0].contains("Retained"));
        assert!(editor("a", "", true).warnings()[0].contains("DELETES"));
        assert!(editor("a", "", false).warnings()[0].contains("empty"));
        assert!(editor("a", "{oops", false).warnings()[0].contains("not valid JSON"));
        assert!(editor("a", "{\"a\":1}", false).warnings().is_empty());
    }

    #[test]
    fn request_trims_the_topic_and_keeps_the_payload_untouched() {
        let request = editor("  a/b ", " {\"x\": 1}\n", true).request();
        assert_eq!(request.topic, "a/b");
        assert_eq!(request.payload, b" {\"x\": 1}\n");
        assert!(request.retain && request.qos == 1);
    }

    #[test]
    fn the_payload_cursor_edits_in_the_middle_and_moves_between_lines() {
        let mut payload = editor("a", "", false);
        payload.insert("ab\ncdef\n\u{e4}");
        assert_eq!(payload.line_col(), (2, 1));

        assert!(payload.move_vertical(-1));
        assert_eq!(payload.line_col(), (1, 1));
        payload.insert("X");
        assert_eq!(payload.payload, "ab\ncXdef\n\u{e4}");
        payload.line_edge(false);
        payload.backspace();
        assert_eq!(payload.payload, "abcXdef\n\u{e4}");
        assert_eq!(payload.line_col(), (0, 2));

        payload.delete();
        assert_eq!(payload.payload, "abXdef\n\u{e4}");
        assert!(!payload.move_vertical(-1), "already on the first line");
        payload.line_edge(true);
        assert_eq!(payload.line_col(), (0, 6));
        assert!(payload.move_vertical(1));
        assert_eq!(
            payload.line_col(),
            (1, 1),
            "column is clamped to the shorter line"
        );
        assert!(!payload.move_vertical(1));
        assert_eq!(payload.line_count(), 2);
    }

    #[test]
    fn json_switches_between_indented_and_one_line_and_other_text_is_refused() {
        let one_line = r#"{"b":[1,2],"a":{"c":null}}"#;
        let mut payload = editor("a", one_line, false);
        payload.toggle_json_format().unwrap();
        assert!(payload.payload.starts_with("{\n  \"b\": ["));
        assert_eq!(payload.cursor, payload.payload.chars().count());
        payload.toggle_json_format().unwrap();
        assert_eq!(
            payload.payload, one_line,
            "the key order survives the round trip"
        );

        // Loose layout (or another indentation) is first normalised, then it toggles as usual.
        let mut loose = editor("a", "{ \"a\" :   1 }", false);
        loose.toggle_json_format().unwrap();
        assert_eq!(loose.payload, "{\n  \"a\": 1\n}");
        loose.toggle_json_format().unwrap();
        assert_eq!(loose.payload, r#"{"a":1}"#);

        let mut text = editor("a", "plain", false);
        assert!(
            text.toggle_json_format()
                .unwrap_err()
                .contains("Not valid JSON")
        );
        assert_eq!(text.payload, "plain");
    }

    #[test]
    fn the_topic_cursor_edits_in_the_middle() {
        let mut topic = editor("plant/line", "", false);
        topic.topic_cursor = 5;
        topic.insert_topic("-x");
        assert_eq!(topic.topic, "plant-x/line");
        assert_eq!(topic.topic_cursor, 7);
        backspace_at(&mut topic.topic, &mut topic.topic_cursor);
        assert_eq!(topic.topic, "plant-/line");
        delete_at(&mut topic.topic, topic.topic_cursor);
        assert_eq!(topic.topic, "plant-line");
        topic.set_topic("a/\u{e4}");
        assert_eq!(topic.topic_cursor, 3);
    }

    #[test]
    fn mistakes_are_never_sent_without_review() {
        assert!(editor("a", "ok", true).needs_review().is_none());
        assert!(
            editor("a", "", true)
                .needs_review()
                .unwrap()
                .contains("deletes")
        );
        assert!(
            editor("a", "{oops", false)
                .needs_review()
                .unwrap()
                .contains("JSON")
        );
        assert!(editor("a", "", false).needs_review().is_none());
    }

    #[test]
    fn payload_files_are_read_as_text_with_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = |name: &str| directory.path().join(name).display().to_string();
        std::fs::write(path("a.txt"), "line 1\nline 2\r\n").unwrap();
        assert_eq!(read_payload_file(&path("a.txt")).unwrap(), "line 1\nline 2");
        std::fs::write(path("empty"), "").unwrap();
        assert_eq!(read_payload_file(&path("empty")).unwrap(), "");
        std::fs::write(path("bin"), [0xff, 0xfe]).unwrap();
        assert!(
            read_payload_file(&path("bin"))
                .unwrap_err()
                .contains("not UTF-8")
        );
        assert!(
            read_payload_file(&path("missing"))
                .unwrap_err()
                .contains("Cannot read")
        );
        assert!(
            read_payload_file(&directory.path().display().to_string())
                .unwrap_err()
                .contains("not a regular file")
        );
        assert!(read_payload_file("  ").is_err());
        let large = vec![b'x'; mate::mqtt::MAX_PUBLISH_PAYLOAD + 1];
        std::fs::write(path("large"), large).unwrap();
        assert!(
            read_payload_file(&path("large"))
                .unwrap_err()
                .contains("larger than")
        );
    }
}
