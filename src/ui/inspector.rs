//! Inspector state: parsed payload cache, tree/raw/hex modes, search and diff. No rendering.

use std::{collections::BTreeSet, sync::Arc};

use mate::{
    diff::{DiffLine, line_diff},
    json_tree::{JsonDoc, JsonRow, ancestors, copy_text},
    topics::Message,
};

/// Containers deeper than this start collapsed when a topic is first inspected.
const DEFAULT_EXPAND_DEPTH: usize = 3;
pub const HEX_BYTES_PER_LINE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Tree,
    Raw,
    Hex,
}

#[derive(Debug)]
pub struct DiffView {
    pub title: String,
    pub lines: Result<Vec<DiffLine>, String>,
    pub scroll: usize,
}

#[derive(Debug, Default)]
pub struct InspectorSearch {
    /// The query is being typed.
    pub active: bool,
    pub query: String,
    pub pointer_matches: Vec<String>,
    pub line_matches: Vec<usize>,
    pub current: usize,
}

impl InspectorSearch {
    pub fn match_count(&self) -> usize {
        self.pointer_matches.len().max(self.line_matches.len())
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub topic: String,
    pub seq: u64,
    pub payload: Arc<[u8]>,
    pub doc: Option<JsonDoc>,
    pub parse_error: Option<String>,
    pub rows: Vec<JsonRow>,
    /// Pretty-printed JSON, or the payload as lossy text for non-JSON data.
    pub text_lines: Vec<String>,
    pub is_utf8: bool,
}

#[derive(Debug)]
pub struct InspectorState {
    pub mode: ViewMode,
    pub expanded: BTreeSet<String>,
    pub cursor: String,
    pub scroll: usize,
    pub scroll_x: usize,
    /// Visible content rows, updated by the renderer.
    pub viewport: usize,
    pub search: InspectorSearch,
    pub diff: Option<DiffView>,
    /// Show the metadata of the selected topic instead of the payload.
    pub info: bool,
    pub info_scroll: usize,
    loaded: Option<Loaded>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            mode: ViewMode::Tree,
            expanded: BTreeSet::new(),
            cursor: String::new(),
            scroll: 0,
            scroll_x: 0,
            viewport: 10,
            search: InspectorSearch::default(),
            diff: None,
            info: false,
            info_scroll: 0,
            loaded: None,
        }
    }
}

impl InspectorState {
    pub fn loaded(&self) -> Option<&Loaded> {
        self.loaded.as_ref()
    }

    /// Reparses only when the displayed message changed; expansion and cursor survive new messages.
    pub fn sync(&mut self, topic: Option<&str>, message: Option<&Message>) {
        let (Some(topic), Some(message)) = (topic, message) else {
            self.loaded = None;
            return;
        };
        if self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.topic == topic && loaded.seq == message.seq)
        {
            return;
        }
        let same_topic = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.topic == topic);

        let (doc, parse_error) = match JsonDoc::parse(&message.payload) {
            Ok(doc) => (Some(doc), None),
            Err(error) => (None, Some(error)),
        };
        let is_utf8 = std::str::from_utf8(&message.payload).is_ok();
        let text_lines = match &doc {
            Some(doc) => doc.pretty().lines().map(str::to_owned).collect(),
            None => String::from_utf8_lossy(&message.payload)
                .lines()
                .map(str::to_owned)
                .collect(),
        };

        if !same_topic {
            self.expanded = doc
                .as_ref()
                .map(|doc| doc.default_expanded(DEFAULT_EXPAND_DEPTH))
                .unwrap_or_default();
            self.cursor.clear();
            self.scroll = 0;
            self.scroll_x = 0;
            self.search = InspectorSearch::default();
            self.diff = None;
        }
        let rows = doc
            .as_ref()
            .map(|doc| doc.rows(&self.expanded))
            .unwrap_or_default();
        self.loaded = Some(Loaded {
            topic: topic.to_owned(),
            seq: message.seq,
            payload: Arc::clone(&message.payload),
            doc,
            parse_error,
            rows,
            text_lines,
            is_utf8,
        });
        if self.search.match_count() > 0 || !self.search.query.is_empty() {
            self.run_search(false);
        }
    }

    pub fn effective_mode(&self) -> ViewMode {
        match &self.loaded {
            Some(loaded) if loaded.doc.is_some() => match self.mode {
                ViewMode::Hex => ViewMode::Raw,
                mode => mode,
            },
            Some(loaded) if !loaded.is_utf8 => match self.mode {
                ViewMode::Raw => ViewMode::Raw,
                _ => ViewMode::Hex,
            },
            _ => match self.mode {
                ViewMode::Tree => ViewMode::Raw,
                mode => mode,
            },
        }
    }

    pub fn toggle_mode(&mut self) {
        let has_doc = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.doc.is_some());
        self.mode = match (self.effective_mode(), has_doc) {
            (ViewMode::Tree, _) => ViewMode::Raw,
            (ViewMode::Raw, true) => ViewMode::Tree,
            (ViewMode::Raw, false) => ViewMode::Hex,
            (ViewMode::Hex, _) => ViewMode::Raw,
        };
        self.scroll = 0;
        self.scroll_x = 0;
        self.search.pointer_matches.clear();
        self.search.line_matches.clear();
        if !self.search.query.is_empty() {
            self.run_search(false);
        }
    }

    pub fn rows(&self) -> &[JsonRow] {
        self.loaded.as_ref().map_or(&[], |loaded| &loaded.rows)
    }

    pub fn cursor_index(&self) -> usize {
        let rows = self.rows();
        if let Some(index) = rows.iter().position(|row| row.pointer == self.cursor) {
            return index;
        }
        // The cursor row vanished after an update: fall back to its closest surviving ancestor.
        ancestors(&self.cursor)
            .iter()
            .rev()
            .find_map(|pointer| rows.iter().position(|row| &row.pointer == pointer))
            .unwrap_or(0)
    }

    pub fn cursor_row(&self) -> Option<&JsonRow> {
        self.rows().get(self.cursor_index())
    }

    fn rebuild_rows(&mut self) {
        if let Some(loaded) = &mut self.loaded
            && let Some(doc) = &loaded.doc
        {
            loaded.rows = doc.rows(&self.expanded);
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        let next = (self.cursor_index() as isize + delta).clamp(0, count as isize - 1) as usize;
        self.cursor = self.rows()[next].pointer.clone();
    }

    pub fn cursor_to_end(&mut self, end: bool) {
        let count = self.rows().len();
        if count > 0 {
            let index = if end { count - 1 } else { 0 };
            self.cursor = self.rows()[index].pointer.clone();
        }
    }

    pub fn expand(&mut self) {
        let Some(row) = self.cursor_row().cloned() else {
            return;
        };
        if !row.expandable {
            return;
        }
        if row.expanded {
            self.move_cursor(1);
        } else {
            self.expanded.insert(row.pointer);
            self.rebuild_rows();
        }
    }

    pub fn collapse(&mut self) {
        let Some(row) = self.cursor_row().cloned() else {
            return;
        };
        if row.expanded {
            self.expanded.remove(&row.pointer);
            self.rebuild_rows();
        } else if let Some((parent, _)) = row.pointer.rsplit_once('/') {
            self.cursor = parent.to_owned();
        }
    }

    pub fn toggle(&mut self) {
        match self.cursor_row() {
            Some(row) if row.expanded => self.collapse(),
            Some(row) if row.expandable => self.expand(),
            _ => {}
        }
    }

    pub fn expand_all(&mut self, expand: bool) {
        self.expanded = match (&self.loaded, expand) {
            (Some(Loaded { doc: Some(doc), .. }), true) => doc.default_expanded(usize::MAX),
            _ => BTreeSet::new(),
        };
        if !expand {
            // The root stays open so the tree never collapses into a single line.
            self.expanded.insert(String::new());
        }
        self.rebuild_rows();
        self.cursor = self
            .rows()
            .first()
            .map(|row| row.pointer.clone())
            .unwrap_or_default();
    }

    pub fn line_count(&self) -> usize {
        match (self.effective_mode(), &self.loaded) {
            (ViewMode::Tree, _) => self.rows().len(),
            (ViewMode::Raw, Some(loaded)) => loaded.text_lines.len(),
            (ViewMode::Hex, Some(loaded)) => loaded.payload.len().div_ceil(HEX_BYTES_PER_LINE),
            _ => 0,
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.line_count().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn scroll_horizontally(&mut self, delta: isize) {
        self.scroll_x = (self.scroll_x as isize + delta).max(0) as usize;
    }

    pub fn begin_search(&mut self) {
        self.search.active = true;
    }

    pub fn clear_search(&mut self) {
        self.search = InspectorSearch::default();
    }

    /// Recomputes matches; `jump` moves the cursor or scroll position to the first match.
    pub fn run_search(&mut self, jump: bool) {
        self.search.pointer_matches.clear();
        self.search.line_matches.clear();
        self.search.current = 0;
        let query = self.search.query.to_lowercase();
        if query.is_empty() {
            return;
        }
        let Some(loaded) = &self.loaded else {
            return;
        };
        match (self.effective_mode(), &loaded.doc) {
            (ViewMode::Tree, Some(doc)) => self.search.pointer_matches = doc.search(&query),
            (ViewMode::Hex, _) => {}
            _ => {
                self.search.line_matches = loaded
                    .text_lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&query))
                    .map(|(index, _)| index)
                    .collect();
            }
        }
        if jump {
            self.jump_to_match();
        }
    }

    pub fn next_match(&mut self, backwards: bool) {
        let count = self.search.match_count();
        if count == 0 {
            return;
        }
        self.search.current = if backwards {
            (self.search.current + count - 1) % count
        } else {
            (self.search.current + 1) % count
        };
        self.jump_to_match();
    }

    fn jump_to_match(&mut self) {
        if let Some(pointer) = self
            .search
            .pointer_matches
            .get(self.search.current)
            .cloned()
        {
            for ancestor in ancestors(&pointer) {
                self.expanded.insert(ancestor);
            }
            self.rebuild_rows();
            self.cursor = pointer;
        } else if let Some(line) = self.search.line_matches.get(self.search.current) {
            self.scroll = *line;
        }
    }

    /// The value under the cursor (tree mode) or the whole text (raw mode).
    pub fn selected_text(&self) -> Option<String> {
        let loaded = self.loaded.as_ref()?;
        match (self.effective_mode(), &loaded.doc) {
            (ViewMode::Tree, Some(doc)) => {
                let row = self.cursor_row()?;
                doc.value_at(&row.pointer).map(copy_text)
            }
            _ => self.payload_text(),
        }
    }

    /// The original payload, unmodified.
    pub fn payload_text(&self) -> Option<String> {
        self.loaded
            .as_ref()
            .map(|loaded| String::from_utf8_lossy(&loaded.payload).into_owned())
    }

    pub fn open_diff(&mut self, title: String, old: &[u8], new: &[u8]) {
        let lines = line_diff(&diff_text(old), &diff_text(new))
            .ok_or_else(|| "payloads are too large to compare".to_owned());
        self.diff = Some(DiffView {
            title,
            lines,
            scroll: 0,
        });
    }
}

fn diff_text(payload: &[u8]) -> String {
    JsonDoc::parse(payload)
        .map(|doc| doc.pretty())
        .unwrap_or_else(|_| String::from_utf8_lossy(payload).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mate::topics::{MessageMeta, TopicStore};

    fn store_with(payload: &[u8]) -> TopicStore {
        let mut topics = TopicStore::default();
        topics.insert_message("a/b", payload.to_vec(), MessageMeta::now());
        topics
    }

    fn synced(topics: &TopicStore) -> InspectorState {
        let mut inspector = InspectorState::default();
        inspector.sync(Some("a/b"), topics.latest_message("a/b"));
        inspector
    }

    const JSON: &[u8] = br#"{"data":{"index":33,"tags":["x","y"]},"header":{"seq":6}}"#;

    #[test]
    fn json_payloads_start_in_tree_mode_with_nested_containers_open() {
        let topics = store_with(JSON);
        let inspector = synced(&topics);
        assert_eq!(inspector.effective_mode(), ViewMode::Tree);
        let paths: Vec<_> = inspector
            .rows()
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert!(paths.contains(&"$.data.index") && paths.contains(&"$.data.tags[1]"));
    }

    #[test]
    fn collapse_and_expand_move_through_the_tree() {
        let topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.move_cursor(1);
        assert_eq!(inspector.cursor, "/data");
        inspector.collapse();
        assert!(
            !inspector
                .rows()
                .iter()
                .any(|row| row.path == "$.data.index")
        );
        inspector.collapse();
        assert_eq!(inspector.cursor, "");
        inspector.move_cursor(1);
        inspector.expand();
        assert!(
            inspector
                .rows()
                .iter()
                .any(|row| row.path == "$.data.index")
        );
        inspector.expand();
        assert_eq!(inspector.cursor, "/data/index");
        inspector.expand_all(false);
        assert_eq!(inspector.rows().len(), 3);
    }

    #[test]
    fn expansion_and_cursor_survive_a_new_message_on_the_same_topic() {
        let mut topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.move_cursor(1);
        inspector.collapse();
        topics.insert(
            "a/b",
            br#"{"data":{"index":34},"header":{"seq":7}}"#.to_vec(),
        );
        inspector.sync(Some("a/b"), topics.latest_message("a/b"));
        assert_eq!(inspector.cursor, "/data");
        assert!(
            !inspector
                .rows()
                .iter()
                .any(|row| row.path == "$.data.index")
        );
        assert!(
            inspector
                .rows()
                .iter()
                .any(|row| row.path == "$.header.seq")
        );
    }

    #[test]
    fn invalid_json_falls_back_to_raw_text_and_binary_to_hex() {
        let topics = store_with(b"{broken");
        let inspector = synced(&topics);
        assert_eq!(inspector.effective_mode(), ViewMode::Raw);
        assert!(inspector.loaded().unwrap().parse_error.is_some());
        assert_eq!(inspector.payload_text().unwrap(), "{broken");

        let topics = store_with(&[0xff, 0x00, 0x10]);
        let mut inspector = synced(&topics);
        assert_eq!(inspector.effective_mode(), ViewMode::Hex);
        inspector.toggle_mode();
        assert_eq!(inspector.effective_mode(), ViewMode::Raw);
    }

    #[test]
    fn toggling_switches_between_tree_and_raw_for_json() {
        let topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.toggle_mode();
        assert_eq!(inspector.effective_mode(), ViewMode::Raw);
        assert!(inspector.line_count() > 3);
        inspector.toggle_mode();
        assert_eq!(inspector.effective_mode(), ViewMode::Tree);
    }

    #[test]
    fn tree_search_expands_ancestors_and_cycles_through_matches() {
        let topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.expand_all(false);
        inspector.search.query = "SEQ".to_owned();
        inspector.run_search(true);
        assert_eq!(inspector.cursor, "/header/seq");
        assert_eq!(inspector.search.match_count(), 1);
        inspector.search.query = "y".to_owned();
        inspector.run_search(true);
        assert_eq!(inspector.cursor, "/data/tags/1");
        inspector.next_match(false);
        assert_eq!(inspector.search.current, 0);
    }

    #[test]
    fn raw_search_scrolls_to_matching_line() {
        let topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.toggle_mode();
        inspector.search.query = "header".to_owned();
        inspector.run_search(true);
        assert_eq!(inspector.search.line_matches.len(), 1);
        assert_eq!(inspector.scroll, inspector.search.line_matches[0]);
    }

    #[test]
    fn copy_returns_selected_value_or_untouched_payload() {
        let topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.cursor = "/data/tags/1".to_owned();
        assert_eq!(inspector.selected_text().unwrap(), "y");
        assert_eq!(inspector.payload_text().unwrap().as_bytes(), JSON);
    }

    #[test]
    fn diff_view_marks_changes_between_payloads() {
        let mut inspector = InspectorState::default();
        inspector.open_diff("t".into(), br#"{"a":1}"#, br#"{"a":2}"#);
        let lines = inspector.diff.as_ref().unwrap().lines.as_ref().unwrap();
        assert!(lines.contains(&DiffLine::Removed("  \"a\": 1".into())));
        assert!(lines.contains(&DiffLine::Added("  \"a\": 2".into())));
    }

    #[test]
    fn switching_topics_resets_view_state() {
        let mut topics = store_with(JSON);
        let mut inspector = synced(&topics);
        inspector.move_cursor(2);
        topics.insert("c/d", br#"{"z":1}"#.to_vec());
        inspector.sync(Some("c/d"), topics.latest_message("c/d"));
        assert_eq!(inspector.cursor, "");
        assert_eq!(inspector.rows().len(), 2);
    }
}
