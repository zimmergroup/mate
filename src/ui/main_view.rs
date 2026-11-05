//! State and keyboard handling of the main view (topic explorer, inspector, history).

use std::{
    collections::{BTreeSet, HashMap},
    time::SystemTime,
};

use chrono::{DateTime, Local};
use crossterm::event::KeyCode;
use mate::{
    fuzzy::match_positions,
    mqtt::{PublishRequest, validate_publish},
    preview::{Preview, payload_preview},
    search::message_matches,
    topics::{Message, TopicRow, TopicStore},
};

use super::{
    UiAction, UiState, View,
    inspector::{InspectorState, ViewMode},
    layout::{LayoutPrefs, Panel},
};

const PREVIEW_CACHE_LIMIT: usize = 4096;
const HORIZONTAL_STEP: isize = 4;

/// A topic counts as favorite if it or one of its ancestors was marked.
fn is_favorite(favorites: &BTreeSet<String>, topic: &str) -> bool {
    favorites.iter().any(|favorite| {
        topic == favorite
            || topic
                .strip_prefix(favorite.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

pub fn clock(wall: SystemTime) -> String {
    DateTime::<Local>::from(wall)
        .format("%H:%M:%S%.3f")
        .to_string()
}

#[derive(Debug, Default)]
pub struct PreviewCache {
    entries: HashMap<String, (u64, Preview)>,
}

impl PreviewCache {
    pub fn get(&mut self, topic: &str, message: &Message) -> &Preview {
        let fresh = self
            .entries
            .get(topic)
            .is_some_and(|(seq, _)| *seq == message.seq);
        if !fresh {
            if self.entries.len() >= PREVIEW_CACHE_LIMIT {
                self.entries.clear();
            }
            self.entries.insert(
                topic.to_owned(),
                (message.seq, payload_preview(&message.payload)),
            );
        }
        &self.entries[topic].1
    }
}

/// The messages of a topic that match `query`, newest first.
pub fn history_messages<'a>(topics: &'a TopicStore, topic: &str, query: &str) -> Vec<&'a Message> {
    topics
        .history(topic)
        .map(|history| {
            history
                .iter()
                .rev()
                .filter(|message| message_matches(message, query))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Default)]
pub struct TopicSearch {
    /// The query is being typed.
    pub active: bool,
    pub query: String,
}

#[derive(Debug)]
pub struct MainView {
    pub focus: Panel,
    pub layout: LayoutPrefs,
    pub terminal_width: u16,
    pub expanded: BTreeSet<String>,
    pub rows: Vec<TopicRow>,
    rows_key: Option<(u64, u64)>,
    pub selected: Option<String>,
    pub selected_index: usize,
    pub topic_scroll: usize,
    pub topic_viewport: usize,
    pub search: TopicSearch,
    /// Filters the history of the selected topic by payload and MQTT 5 properties.
    pub history_search: TopicSearch,
    filter: Option<BTreeSet<String>>,
    filter_key: Option<(u64, String, u64, bool)>,
    filter_generation: u64,
    pub favorites: BTreeSet<String>,
    /// Bumped whenever the favorites change so a favorites filter is recomputed.
    favorites_revision: u64,
    pub favorites_only: bool,
    pub previews: PreviewCache,
    pub history_cursor: Option<u64>,
    pub history_scroll: usize,
    pub history_viewport: usize,
    /// `Some` while a historical message is shown instead of the live one.
    pub pinned: Option<u64>,
    pub compare_base: Option<u64>,
    pub inspector: InspectorState,
}

impl Default for MainView {
    fn default() -> Self {
        Self {
            focus: Panel::Topics,
            layout: LayoutPrefs::default(),
            terminal_width: 120,
            expanded: BTreeSet::new(),
            rows: Vec::new(),
            rows_key: None,
            selected: None,
            selected_index: 0,
            topic_scroll: 0,
            topic_viewport: 10,
            search: TopicSearch::default(),
            history_search: TopicSearch::default(),
            filter: None,
            filter_key: None,
            filter_generation: 0,
            favorites: BTreeSet::new(),
            favorites_revision: 0,
            favorites_only: false,
            previews: PreviewCache::default(),
            history_cursor: None,
            history_scroll: 0,
            history_viewport: 10,
            pinned: None,
            compare_base: None,
            inspector: InspectorState::default(),
        }
    }
}

impl MainView {
    /// Drops everything derived from the data of a previous broker; layout and focus stay.
    pub fn reset_for_new_data(&mut self) {
        let layout = self.layout.clone();
        let focus = self.focus;
        let width = self.terminal_width;
        *self = Self {
            layout,
            focus,
            terminal_width: width,
            ..Self::default()
        };
    }

    pub fn filter_active(&self) -> bool {
        !self.search.query.trim().is_empty() || self.favorites_only
    }

    pub fn toggle_favorite(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if !self.favorites.remove(&path) {
            self.favorites.insert(path);
        }
        self.favorites_revision += 1;
    }

    pub fn toggle_favorites_only(&mut self) {
        self.favorites_only = !self.favorites_only;
    }

    pub fn clear_filter(&mut self) {
        self.search.query.clear();
        self.favorites_only = false;
    }

    fn reset_message_view(&mut self) {
        self.history_cursor = None;
        self.history_scroll = 0;
        self.pinned = None;
        self.compare_base = None;
    }

    /// Rebuilds the visible rows only when the tree structure, filter or expansion changed.
    pub fn ensure_rows(&mut self, topics: &TopicStore) {
        let revision = topics.structure_revision();
        let query = self.search.query.trim().to_owned();
        if !self.filter_active() {
            if self.filter.take().is_some() {
                self.filter_generation += 1;
            }
            self.filter_key = None;
        } else if self
            .filter_key
            .as_ref()
            .is_none_or(|(known, text, favorites, only)| {
                *known != revision
                    || *text != query
                    || *favorites != self.favorites_revision
                    || *only != self.favorites_only
            })
        {
            let (favorites, only) = (&self.favorites, self.favorites_only);
            self.filter = Some(topics.matching_paths(|topic| {
                (query.is_empty() || match_positions(&query, topic).is_some())
                    && (!only || is_favorite(favorites, topic))
            }));
            self.filter_key = Some((
                revision,
                query,
                self.favorites_revision,
                self.favorites_only,
            ));
            self.filter_generation += 1;
        }

        let key = (revision, self.filter_generation);
        if self.rows_key == Some(key) {
            return;
        }
        self.rows = topics.visible_rows_filtered(&self.expanded, self.filter.as_ref());
        self.rows_key = Some(key);
        self.repair_selection();
    }

    fn invalidate_rows(&mut self) {
        self.rows_key = None;
    }

    /// Keeps the selection by path; falls back to the closest visible ancestor, then the first row.
    fn repair_selection(&mut self) {
        let position = |rows: &[TopicRow], path: &str| rows.iter().position(|row| row.path == path);
        let target = self.selected.as_deref().and_then(|selected| {
            if let Some(index) = position(&self.rows, selected) {
                return Some(index);
            }
            let mut candidate = selected;
            while let Some((parent, _)) = candidate.rsplit_once('/') {
                if let Some(index) = position(&self.rows, parent) {
                    return Some(index);
                }
                candidate = parent;
            }
            None
        });
        match target.or((!self.rows.is_empty()).then_some(0)) {
            Some(index) => self.select_index(index),
            None => {
                self.select_path(None);
                self.selected_index = 0;
            }
        }
    }

    /// Expands the ancestors of `path`, drops filters that could hide it and selects it.
    pub fn reveal(&mut self, path: &str) {
        let mut prefix = String::new();
        let segments: Vec<&str> = path.split('/').collect();
        for segment in &segments[..segments.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            self.expanded.insert(prefix.clone());
        }
        self.clear_filter();
        self.select_path(Some(path.to_owned()));
        self.invalidate_rows();
    }

    fn select_path(&mut self, path: Option<String>) {
        if self.selected != path {
            self.selected = path;
            self.reset_message_view();
        }
    }

    fn select_index(&mut self, index: usize) {
        let Some(path) = self.rows.get(index).map(|row| row.path.clone()) else {
            return;
        };
        self.selected_index = index;
        self.select_path(Some(path));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let next = (self.selected_index as isize + delta).clamp(0, last);
        self.select_index(next as usize);
    }

    fn expand_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected_index).cloned() else {
            return;
        };
        if !row.has_children {
            return;
        }
        if self.expanded.contains(&row.path) || self.filter_active() {
            self.move_selection(1);
        } else {
            self.expanded.insert(row.path);
            self.invalidate_rows();
        }
    }

    fn collapse_selected(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        if self.expanded.remove(&path) && !self.filter_active() {
            self.invalidate_rows();
        } else if let Some((parent, _)) = path.rsplit_once('/')
            && let Some(index) = self.rows.iter().position(|row| row.path == parent)
        {
            self.select_index(index);
        }
    }

    pub fn expand_all(&mut self, topics: &TopicStore) {
        self.expanded = topics.branch_paths();
        self.invalidate_rows();
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.invalidate_rows();
    }

    /// Shift+Right / Shift+Left: opens or closes the selected branch with everything below it.
    pub fn set_subtree(&mut self, topics: &TopicStore, expand: bool) {
        let Some(row) = self.rows.get(self.selected_index).cloned() else {
            return;
        };
        // A leaf has no subtree of its own, so closing acts on its parent.
        let target = if row.has_children {
            row.path.clone()
        } else if expand {
            return;
        } else if let Some((parent, _)) = row.path.rsplit_once('/') {
            parent.to_owned()
        } else {
            return;
        };
        if expand {
            self.expanded.extend(topics.branch_paths_below(&target));
        } else {
            let prefix = format!("{target}/");
            self.expanded.remove(&target);
            self.expanded.retain(|path| !path.starts_with(&prefix));
            self.select_path(Some(target));
        }
        self.invalidate_rows();
    }

    fn toggle_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected_index) else {
            return;
        };
        if self.filter_active() {
            return;
        }
        let path = row.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.invalidate_rows();
    }
}

impl UiState {
    pub(super) fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(super::Notice {
            text: text.into(),
            at: self.now,
            error,
        });
    }

    /// The message shown in the inspector: the pinned historical one, otherwise the latest.
    pub fn displayed_message(&self) -> Option<&Message> {
        let topic = self.main.selected.as_deref()?;
        match self.main.pinned {
            Some(seq) => self.topics.find_message(topic, seq),
            None => self.topics.latest_message(topic),
        }
    }

    /// Brings derived view state in line with the data; called before every render.
    pub(super) fn prepare_frame(&mut self) {
        self.main.ensure_rows(&self.topics);
        if self.view == View::Analytics {
            self.refresh_analytics();
        }
        if self.view == View::Stream {
            self.refresh_stream();
        }
        if let (Some(topic), Some(seq)) = (self.main.selected.as_deref(), self.main.pinned)
            && self.topics.find_message(topic, seq).is_none()
        {
            self.main.pinned = None;
            self.notify(
                "The inspected message was dropped from the history; showing live data",
                false,
            );
        }
        let topic = self.main.selected.clone();
        let message = match (topic.as_deref(), self.main.pinned) {
            (Some(topic), Some(seq)) => self.topics.find_message(topic, seq),
            (Some(topic), None) => self.topics.latest_message(topic),
            _ => None,
        };
        self.main.inspector.sync(topic.as_deref(), message);
    }

    pub(super) fn handle_main_key(&mut self, code: KeyCode) -> UiAction {
        self.main.ensure_rows(&self.topics);
        let action = self.dispatch_main_key(code);
        self.main.ensure_rows(&self.topics);
        action
    }

    fn dispatch_main_key(&mut self, code: KeyCode) -> UiAction {
        if self.main.search.active {
            self.topic_search_key(code);
            return UiAction::None;
        }
        if self.main.inspector.search.active {
            self.inspector_search_key(code);
            return UiAction::None;
        }
        if self.main.history_search.active {
            self.history_search_key(code);
            return UiAction::None;
        }
        match code {
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Tab => self.cycle_focus(false),
            KeyCode::BackTab => self.cycle_focus(true),
            KeyCode::Char('m') => {
                let focus = self.main.focus;
                self.main.layout.maximized =
                    (self.main.layout.maximized != Some(focus)).then_some(focus);
            }
            KeyCode::Char('h') => self.toggle_history(),
            KeyCode::Char('<') => self.resize_focused(-1),
            KeyCode::Char('>') => self.resize_focused(1),
            KeyCode::Char('b') => {
                self.refresh_broker_details();
                self.view = View::Brokers;
            }
            KeyCode::Char('o') => return self.connect_selected(),
            KeyCode::Char('p') => self.open_publish_editor(),
            KeyCode::Char('P') => self.toggle_publish_panel(),
            KeyCode::Char('x') => return UiAction::Disconnect,
            KeyCode::Char('A') => self.open_analytics(),
            KeyCode::Char('S') => self.open_stream(),
            KeyCode::Char('R') => self.open_replay(),
            KeyCode::Char('t') => self.copy_topic(),
            KeyCode::Char('i') => {
                let inspector = &mut self.main.inspector;
                inspector.info = !inspector.info;
                inspector.info_scroll = 0;
            }
            KeyCode::Esc => self.escape(),
            _ => match self.main.focus {
                Panel::Topics => self.topics_key(code),
                Panel::Inspector => self.inspector_key(code),
                Panel::History => self.history_key(code),
                Panel::Publish => {}
            },
        }
        UiAction::None
    }

    /// Shift+arrows in the topic panel.
    pub(super) fn topic_subtree(&mut self, expand: bool) {
        self.main.ensure_rows(&self.topics);
        self.main.set_subtree(&self.topics, expand);
        self.main.ensure_rows(&self.topics);
    }

    pub(super) fn cycle_focus(&mut self, backwards: bool) {
        let next = self.main.layout.next_panel(self.main.focus, backwards);
        self.main.focus = next;
        if self.main.layout.maximized.is_some() {
            self.main.layout.maximized = Some(next);
        }
    }

    fn toggle_history(&mut self) {
        let layout = &mut self.main.layout;
        layout.show_history = !layout.show_history;
        if !layout.show_history {
            if self.main.focus == Panel::History {
                self.main.focus = Panel::Inspector;
            }
            if layout.maximized == Some(Panel::History) {
                layout.maximized = Some(Panel::Inspector);
            }
        }
    }

    pub(super) fn resize_focused(&mut self, direction: i16) {
        let width = self.main.terminal_width;
        let focus = self.main.focus;
        if !self.main.layout.resize(focus, direction, width) {
            self.notify("Panel width cannot change further here", false);
        }
    }

    fn escape(&mut self) {
        let inspector = &mut self.main.inspector;
        if inspector.diff.is_some() {
            inspector.diff = None;
        } else if inspector.info {
            inspector.info = false;
        } else if inspector.search.match_count() > 0 || !inspector.search.query.is_empty() {
            inspector.clear_search();
        } else if self.main.focus == Panel::History && !self.main.history_search.query.is_empty() {
            self.main.history_search.query.clear();
            self.main.history_cursor = None;
            self.main.history_scroll = 0;
        } else if self.main.layout.maximized.is_some() {
            self.main.layout.maximized = None;
        } else if self.main.filter_active() {
            self.main.clear_filter();
        } else if self.main.pinned.is_some() {
            self.main.pinned = None;
        } else if self.main.compare_base.is_some() {
            self.main.compare_base = None;
        }
    }

    fn topic_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.main.search.active = false;
                self.main.search.query.clear();
            }
            KeyCode::Enter => self.main.search.active = false,
            KeyCode::Backspace => {
                self.main.search.query.pop();
            }
            KeyCode::Down => self.main.move_selection(1),
            KeyCode::Up => self.main.move_selection(-1),
            KeyCode::Char(character) => self.main.search.query.push(character),
            _ => {}
        }
        self.main.ensure_rows(&self.topics);
    }

    fn history_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.main.history_search.active = false;
                self.main.history_search.query.clear();
            }
            KeyCode::Enter => self.main.history_search.active = false,
            KeyCode::Backspace => {
                self.main.history_search.query.pop();
            }
            KeyCode::Down => self.move_history_cursor(1),
            KeyCode::Up => self.move_history_cursor(-1),
            KeyCode::Char(character) => self.main.history_search.query.push(character),
            _ => {}
        }
        if matches!(code, KeyCode::Esc | KeyCode::Backspace | KeyCode::Char(_)) {
            self.main.history_cursor = None;
            self.main.history_scroll = 0;
        }
    }

    fn inspector_search_key(&mut self, code: KeyCode) {
        let inspector = &mut self.main.inspector;
        match code {
            KeyCode::Esc => inspector.clear_search(),
            KeyCode::Enter => inspector.search.active = false,
            KeyCode::Backspace => {
                inspector.search.query.pop();
                inspector.run_search(true);
            }
            KeyCode::Char(character) => {
                inspector.search.query.push(character);
                inspector.run_search(true);
            }
            _ => {}
        }
    }

    fn topics_key(&mut self, code: KeyCode) {
        let page = self.main.topic_viewport.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.main.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.main.move_selection(1),
            KeyCode::PageUp => self.main.move_selection(-page),
            KeyCode::PageDown => self.main.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => self.main.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.main.move_selection(isize::MAX / 2),
            KeyCode::Right | KeyCode::Char('l') => self.main.expand_selected(),
            KeyCode::Left => self.main.collapse_selected(),
            KeyCode::Enter => {
                let Some((has_children, has_value)) = self
                    .main
                    .rows
                    .get(self.main.selected_index)
                    .map(|row| (row.has_children, row.has_value))
                else {
                    return;
                };
                if has_children {
                    self.main.toggle_selected();
                } else if has_value {
                    self.main.focus = Panel::Inspector;
                    if self.main.layout.maximized.is_some() {
                        self.main.layout.maximized = Some(Panel::Inspector);
                    }
                }
            }
            KeyCode::Char('/') => self.main.search.active = true,
            KeyCode::Char('*') => self.main.toggle_favorite(),
            KeyCode::Char('F') => self.main.toggle_favorites_only(),
            KeyCode::Char('+') => self.main.expand_all(&self.topics),
            KeyCode::Char('-') => self.main.collapse_all(),
            KeyCode::Char('c' | 'C') => self.copy_payload(),
            _ => {}
        }
        self.main.ensure_rows(&self.topics);
    }

    fn inspector_key(&mut self, code: KeyCode) {
        let page = self.main.inspector.viewport.saturating_sub(1).max(1) as isize;
        if let Some(diff) = &mut self.main.inspector.diff {
            match code {
                KeyCode::Up | KeyCode::Char('k') => diff.scroll = diff.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => diff.scroll += 1,
                KeyCode::PageUp => diff.scroll = diff.scroll.saturating_sub(page as usize),
                KeyCode::PageDown => diff.scroll += page as usize,
                KeyCode::Home | KeyCode::Char('g') => diff.scroll = 0,
                KeyCode::Char('d') => self.main.inspector.diff = None,
                _ => {}
            }
            return;
        }
        let tree = self.main.inspector.effective_mode() == ViewMode::Tree;
        let inspector = &mut self.main.inspector;
        if inspector.info {
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    inspector.info_scroll = inspector.info_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => inspector.info_scroll += 1,
                KeyCode::PageUp => {
                    inspector.info_scroll = inspector.info_scroll.saturating_sub(page as usize)
                }
                KeyCode::PageDown => inspector.info_scroll += page as usize,
                KeyCode::Home | KeyCode::Char('g') => inspector.info_scroll = 0,
                KeyCode::Char('c') => {
                    let text = inspector.payload_text();
                    self.copy(text, "payload");
                }
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') if tree => inspector.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') if tree => inspector.move_cursor(1),
            KeyCode::PageUp if tree => inspector.move_cursor(-page),
            KeyCode::PageDown if tree => inspector.move_cursor(page),
            KeyCode::Home | KeyCode::Char('g') if tree => inspector.cursor_to_end(false),
            KeyCode::End | KeyCode::Char('G') if tree => inspector.cursor_to_end(true),
            KeyCode::Right | KeyCode::Char('l') if tree => inspector.expand(),
            KeyCode::Left if tree => inspector.collapse(),
            KeyCode::Enter if tree => inspector.toggle(),
            KeyCode::Char('+') if tree => inspector.expand_all(true),
            KeyCode::Char('-') if tree => inspector.expand_all(false),
            KeyCode::Up | KeyCode::Char('k') => inspector.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => inspector.scroll_by(1),
            KeyCode::PageUp => inspector.scroll_by(-page),
            KeyCode::PageDown => inspector.scroll_by(page),
            KeyCode::Home | KeyCode::Char('g') => inspector.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => inspector.scroll_by(isize::MAX / 2),
            KeyCode::Left => inspector.scroll_horizontally(-HORIZONTAL_STEP),
            KeyCode::Right | KeyCode::Char('l') => inspector.scroll_horizontally(HORIZONTAL_STEP),
            KeyCode::Char('0') => inspector.scroll_x = 0,
            KeyCode::Char('r') => inspector.toggle_mode(),
            KeyCode::Char('/') => {
                inspector.search.query.clear();
                inspector.begin_search();
            }
            KeyCode::Char('n') => inspector.next_match(false),
            KeyCode::Char('N') => inspector.next_match(true),
            KeyCode::Char('c') => {
                let text = inspector.selected_text();
                self.copy(text, "value");
            }
            KeyCode::Char('C') => {
                let text = inspector.payload_text();
                self.copy(text, "payload");
            }
            KeyCode::Char('d') => self.diff_with_previous(),
            _ => {}
        }
    }

    pub(super) fn copy(&mut self, text: Option<String>, what: &str) {
        let Some(text) = text else {
            self.notify("Nothing to copy", true);
            return;
        };
        self.pending_clipboard = Some(super::ClipboardRequest {
            text,
            what: what.to_owned(),
        });
    }

    /// The payload of the message in the history cursor (history panel) or the inspected one.
    fn payload_for_copy(&self) -> Option<String> {
        let message = if self.main.focus == Panel::History {
            let topic = self.main.selected.as_deref()?;
            let seq = self.history_cursor_seq()?;
            self.topics.find_message(topic, seq)
        } else {
            self.displayed_message()
        };
        message.map(|message| String::from_utf8_lossy(&message.payload).into_owned())
    }

    /// Ctrl+S on a history message (or the pinned one): sends it again as it was received.
    pub(super) fn republish_from_history(&mut self) -> UiAction {
        let in_history = self.main.focus == Panel::History;
        let on_pinned = self.main.focus == Panel::Inspector && self.main.pinned.is_some();
        if !in_history && !on_pinned {
            return UiAction::None;
        }
        if let Some(reason) = self.publish_blocker() {
            self.notify(reason, true);
            return UiAction::None;
        }
        let Some(topic) = self.main.selected.clone() else {
            return UiAction::None;
        };
        let message = if in_history {
            self.history_cursor_seq()
                .and_then(|seq| self.topics.find_message(&topic, seq))
        } else {
            self.displayed_message()
        };
        let Some(message) = message else {
            self.notify("Select a message in the history first", true);
            return UiAction::None;
        };
        let broker = message.broker.as_deref().map(str::to_owned);
        let request = PublishRequest {
            topic,
            payload: message.payload.to_vec(),
            qos: message.qos,
            retain: message.retain,
            user_properties: message
                .properties
                .as_ref()
                .map(|properties| properties.user_properties.clone())
                .unwrap_or_default(),
        };
        // A message without a known origin goes to the broker a new message would use.
        let Some(broker) = broker.or_else(|| self.default_publish_broker()) else {
            return UiAction::None;
        };
        match validate_publish(&request) {
            Ok(()) => UiAction::Publish(broker, request),
            Err(error) => {
                self.notify(format!("Not published: {error:#}"), true);
                UiAction::None
            }
        }
    }

    fn copy_payload(&mut self) {
        let text = self.payload_for_copy();
        self.copy(text, "payload");
    }

    fn copy_topic(&mut self) {
        let topic = self.main.selected.clone();
        self.copy(topic, "topic");
    }

    /// Ctrl+C: the value under the inspector cursor, otherwise the payload of the shown message.
    pub(super) fn copy_in_context(&mut self) {
        self.main.ensure_rows(&self.topics);
        let in_tree = self.main.focus == Panel::Inspector && self.main.inspector.diff.is_none();
        if in_tree {
            let text = self.main.inspector.selected_text();
            self.copy(text, "value");
        } else {
            self.copy_payload();
        }
    }

    fn diff_with_previous(&mut self) {
        let topic = self.main.selected.clone();
        let current = self
            .displayed_message()
            .map(|message| (message.seq, message.wall, message.payload.clone()));
        let (Some(topic), Some((seq, wall, payload))) = (topic, current) else {
            self.notify("Select a topic with a message first", true);
            return;
        };
        let Some(previous) = self.topics.previous_message(&topic, seq) else {
            self.notify("No earlier message is retained for this topic", true);
            return;
        };
        let title = format!("{} → {}", clock(previous.wall), clock(wall));
        let old = previous.payload.clone();
        self.main.inspector.open_diff(title, &old, &payload);
    }

    fn history_key(&mut self, code: KeyCode) {
        let page = self.main.history_viewport.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_history_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_history_cursor(1),
            KeyCode::PageUp => self.move_history_cursor(-page),
            KeyCode::PageDown => self.move_history_cursor(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_history_cursor(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_history_cursor(isize::MAX / 2),
            KeyCode::Enter => {
                self.main.pinned = self.history_cursor_seq();
                if self.main.pinned.is_some() {
                    self.main.inspector.diff = None;
                }
            }
            KeyCode::Char('l') => {
                self.main.pinned = None;
                self.main.history_cursor = None;
                self.main.history_scroll = 0;
            }
            KeyCode::Char('d') => self.compare_from_history(),
            KeyCode::Char('/') => self.main.history_search.active = true,
            KeyCode::Char('c' | 'C') => self.copy_payload(),
            _ => {}
        }
    }

    /// Cursor position in the newest-first list; without an explicit cursor it sits on the newest message.
    pub(super) fn history_cursor_index(&self) -> usize {
        let (Some(topic), Some(seq)) = (self.main.selected.as_deref(), self.main.history_cursor)
        else {
            return 0;
        };
        history_messages(&self.topics, topic, &self.main.history_search.query)
            .iter()
            .position(|message| message.seq == seq)
            .unwrap_or(0)
    }

    pub(super) fn history_cursor_seq(&self) -> Option<u64> {
        let topic = self.main.selected.as_deref()?;
        history_messages(&self.topics, topic, &self.main.history_search.query)
            .get(self.history_cursor_index())
            .map(|message| message.seq)
    }

    fn move_history_cursor(&mut self, delta: isize) {
        let Some(topic) = self.main.selected.as_deref() else {
            return;
        };
        let messages = history_messages(&self.topics, topic, &self.main.history_search.query);
        if messages.is_empty() {
            return;
        }
        let last = messages.len() as isize - 1;
        let next = (self.history_cursor_index() as isize + delta).clamp(0, last) as usize;
        let seq = messages[next].seq;
        self.main.history_cursor = Some(seq);
    }

    fn compare_from_history(&mut self) {
        let (Some(topic), Some(seq)) = (self.main.selected.clone(), self.history_cursor_seq())
        else {
            return;
        };
        let Some(base) = self.main.compare_base else {
            self.main.compare_base = Some(seq);
            self.notify(
                "Compare base set; select another message and press d",
                false,
            );
            return;
        };
        if base == seq {
            self.main.compare_base = None;
            self.notify("Compare cancelled", false);
            return;
        }
        let (older, newer) = (base.min(seq), base.max(seq));
        let (Some(old), Some(new)) = (
            self.topics.find_message(&topic, older).cloned(),
            self.topics.find_message(&topic, newer).cloned(),
        ) else {
            self.main.compare_base = None;
            self.notify("The compare base was dropped from the history", true);
            return;
        };
        let title = format!("{} → {}", clock(old.wall), clock(new.wall));
        self.main
            .inspector
            .open_diff(title, &old.payload, &new.payload);
        self.main.compare_base = None;
    }
}
