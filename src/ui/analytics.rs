//! The analytics view: several rankings over the topics MATE has seen.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use crossterm::event::KeyCode;
use mate::{
    analytics::{
        IntervalStats, PayloadClass, PayloadKind, classify_payload, interval_stats, window_rate,
    },
    fuzzy::match_positions,
    topics::TopicStore,
};

use super::{UiAction, UiState, View, layout::Panel};

/// The list is cut here; at 300,000 topics a longer one is neither readable nor cheap.
pub const MAX_ROWS: usize = 2000;
/// Ranking and classification are recomputed at most this often while the view is open.
const REFRESH: Duration = Duration::from_secs(1);
const DETAIL_CHARS: usize = 120;
pub const MAX_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Frequency,
    InvalidJson,
    Quiet,
    Jitter,
    Largest,
    Retained,
    Branches,
}

impl Mode {
    pub const ALL: [Mode; 7] = [
        Mode::Frequency,
        Mode::InvalidJson,
        Mode::Quiet,
        Mode::Jitter,
        Mode::Largest,
        Mode::Retained,
        Mode::Branches,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Frequency => "Frequency",
            Mode::InvalidJson => "Not JSON",
            Mode::Quiet => "Quiet",
            Mode::Jitter => "Jitter",
            Mode::Largest => "Largest",
            Mode::Retained => "Retained",
            Mode::Branches => "Branches",
        }
    }

    /// The first key is the default.
    pub fn sort_keys(self) -> &'static [SortKey] {
        match self {
            Mode::Frequency => &[SortKey::Count, SortKey::Rate],
            Mode::InvalidJson => &[SortKey::Count, SortKey::Size],
            Mode::Quiet => &[SortKey::Age, SortKey::Overdue],
            Mode::Jitter => &[SortKey::Jitter, SortKey::MaxInterval],
            Mode::Largest => &[SortKey::Size, SortKey::Peak, SortKey::Count],
            Mode::Retained => &[SortKey::Age, SortKey::Size],
            Mode::Branches => &[SortKey::Count, SortKey::Rate, SortKey::Topics],
        }
    }

    fn step(self, forward: bool) -> Mode {
        let index = Mode::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        let count = Mode::ALL.len();
        Mode::ALL[if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        }]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Count,
    Rate,
    /// Time since the last message.
    Age,
    /// Time since the last message relative to the topic's usual interval.
    Overdue,
    Jitter,
    MaxInterval,
    Size,
    /// Largest payload in the retained history.
    Peak,
    Topics,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Count => "message count",
            SortKey::Rate => "message rate",
            SortKey::Age => "silence",
            SortKey::Overdue => "overdue factor",
            SortKey::Jitter => "jitter",
            SortKey::MaxInterval => "longest interval",
            SortKey::Size => "payload size",
            SortKey::Peak => "peak payload size",
            SortKey::Topics => "number of topics",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub topic: String,
    /// Messages since the last connection attempt.
    pub count: u64,
    pub rate: Option<f64>,
    /// Usual (mean) time between messages.
    pub interval: Option<Duration>,
    pub min_interval: Option<Duration>,
    pub max_interval: Option<Duration>,
    pub jitter: Option<f64>,
    pub received: Instant,
    /// Time since the last message divided by the usual interval.
    pub overdue: Option<f64>,
    pub size: usize,
    pub peak: usize,
    /// Branches: number of topics in the group.
    pub topics: usize,
    pub kind: PayloadKind,
    /// The parser's message for broken JSON, otherwise the start of the payload.
    pub detail: String,
}

impl Row {
    fn new(topic: &str, received: Instant) -> Self {
        Self {
            topic: topic.to_owned(),
            count: 0,
            rate: None,
            interval: None,
            min_interval: None,
            max_interval: None,
            jitter: None,
            received,
            overdue: None,
            size: 0,
            peak: 0,
            topics: 1,
            kind: PayloadKind::Json,
            detail: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct AnalyticsView {
    pub mode: Mode,
    pub sort: SortKey,
    pub descending: bool,
    pub kind_filter: Option<PayloadKind>,
    /// Branches: how many topic levels form a group.
    pub depth: usize,
    pub selection: usize,
    pub scroll: usize,
    /// Visible list rows, updated by the renderer.
    pub viewport: usize,
    pub rows: Vec<Row>,
    /// Rows before the display limit.
    pub matching: usize,
    /// Cached topics without a message since the last connection attempt (frequency mode).
    pub idle: usize,
    /// Fuzzy filter on the topic name, like the explorer's.
    pub filter: String,
    /// The filter is being typed.
    pub filtering: bool,
    /// Rows are not refreshed over time; options still recompute them.
    pub paused: bool,
    computed_at: Option<Instant>,
    stale: bool,
    classes: HashMap<String, (u64, PayloadClass)>,
}

impl Default for AnalyticsView {
    fn default() -> Self {
        Self {
            mode: Mode::Frequency,
            sort: SortKey::Count,
            descending: true,
            kind_filter: None,
            depth: 1,
            selection: 0,
            scroll: 0,
            viewport: 10,
            rows: Vec::new(),
            matching: 0,
            idle: 0,
            filter: String::new(),
            filtering: false,
            paused: false,
            computed_at: None,
            stale: true,
            classes: HashMap::new(),
        }
    }
}

impl AnalyticsView {
    /// The topic data was replaced; cached classifications no longer apply.
    pub fn reset(&mut self) {
        self.classes.clear();
        self.paused = false;
        self.rows.clear();
        self.selection = 0;
        self.scroll = 0;
        self.invalidate();
    }

    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    pub fn selected_topic(&self) -> Option<&str> {
        self.rows.get(self.selection).map(|row| row.topic.as_str())
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.sort = mode.sort_keys()[0];
        self.descending = true;
        self.restart_list();
    }

    fn restart_list(&mut self) {
        self.selection = 0;
        self.scroll = 0;
        self.invalidate();
    }

    fn cycle_sort(&mut self) {
        let keys = self.mode.sort_keys();
        let next = keys
            .iter()
            .position(|key| *key == self.sort)
            .map_or(0, |index| (index + 1) % keys.len());
        self.sort = keys[next];
        self.invalidate();
    }

    fn cycle_filter(&mut self) {
        self.kind_filter = match self.kind_filter {
            None => Some(PayloadKind::BrokenJson),
            Some(PayloadKind::BrokenJson) => Some(PayloadKind::Text),
            Some(PayloadKind::Text) => Some(PayloadKind::Binary),
            Some(PayloadKind::Binary) => Some(PayloadKind::Empty),
            Some(_) => None,
        };
        self.restart_list();
    }

    /// The moment the ages are measured from: frozen while paused.
    pub fn reference(&self, now: Instant) -> Instant {
        match self.computed_at {
            Some(at) if self.paused => at,
            _ => now,
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.invalidate();
    }

    fn edit_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(character) => self.filter.push(character),
            _ => return,
        }
        if code != KeyCode::Enter {
            self.restart_list();
        }
    }

    fn cycle_depth(&mut self) {
        self.depth = self.depth % MAX_DEPTH + 1;
        self.restart_list();
    }
}

pub(super) fn payload_preview(payload: &[u8]) -> String {
    match std::str::from_utf8(payload) {
        Ok(text) => text
            .chars()
            .take(DETAIL_CHARS)
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect(),
        Err(_) => payload
            .iter()
            .take(24)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn detail_text(class: &PayloadClass, payload: &[u8]) -> String {
    class
        .error
        .clone()
        .unwrap_or_else(|| payload_preview(payload))
}

fn compare(a: &Row, b: &Row, sort: SortKey) -> Ordering {
    let by_count = a.count.cmp(&b.count);
    let by_rate = a.rate.partial_cmp(&b.rate).unwrap_or(Ordering::Equal);
    // Older (smaller Instant) means a longer silence, which ranks higher.
    let by_age = b.received.cmp(&a.received);
    match sort {
        SortKey::Count => by_count.then(by_rate),
        SortKey::Rate => by_rate.then(by_count),
        SortKey::Age => by_age,
        SortKey::Overdue => a
            .overdue
            .partial_cmp(&b.overdue)
            .unwrap_or(Ordering::Equal)
            .then(by_age),
        SortKey::Jitter => a.jitter.partial_cmp(&b.jitter).unwrap_or(Ordering::Equal),
        SortKey::MaxInterval => a.max_interval.cmp(&b.max_interval),
        SortKey::Size => a.size.cmp(&b.size).then(a.peak.cmp(&b.peak)),
        SortKey::Peak => a.peak.cmp(&b.peak).then(a.size.cmp(&b.size)),
        SortKey::Topics => a.topics.cmp(&b.topics).then(by_count),
    }
}

/// The first `depth` levels of `path`, or all of it for a shorter topic.
fn branch_key(path: &str, depth: usize) -> &str {
    path.match_indices('/')
        .nth(depth - 1)
        .map_or(path, |(index, _)| &path[..index])
}

fn topic_matches(filter: &str, topic: &str) -> bool {
    filter.is_empty() || match_positions(filter, topic).is_some()
}

fn branch_rows(depth: usize, filter: &str, topics: &TopicStore) -> Vec<Row> {
    struct Group {
        topics: usize,
        count: u64,
        rate: f64,
        size: usize,
        received: Instant,
    }
    let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
    for snapshot in topics.snapshots() {
        if !topic_matches(filter, snapshot.path) {
            continue;
        }
        let group = groups
            .entry(branch_key(snapshot.path, depth))
            .or_insert(Group {
                topics: 0,
                count: 0,
                rate: 0.0,
                size: 0,
                received: snapshot.latest.received,
            });
        group.topics += 1;
        group.count += snapshot.session_count;
        group.rate += window_rate(snapshot.window_len, snapshot.window_span).unwrap_or(0.0);
        group.size += snapshot.latest.payload.len();
        group.received = group.received.max(snapshot.latest.received);
    }
    groups
        .into_iter()
        .map(|(path, group)| {
            let mut row = Row::new(path, group.received);
            row.topics = group.topics;
            row.count = group.count;
            row.rate = (group.rate > 0.0).then_some(group.rate);
            row.size = group.size;
            row
        })
        .collect()
}

/// What a mode adds to a row beyond the common numbers.
struct Extra {
    kind: PayloadKind,
    detail: String,
    stats: Option<IntervalStats>,
    peak: usize,
}

fn collect_rows(view: &mut AnalyticsView, topics: &TopicStore, now: Instant) -> (Vec<Row>, usize) {
    let filter = view.filter.trim().to_owned();
    if view.mode == Mode::Branches {
        return (branch_rows(view.depth, &filter, topics), 0);
    }
    let mut rows = Vec::new();
    let mut idle = 0;
    for snapshot in topics.snapshots() {
        if !topic_matches(&filter, snapshot.path) {
            continue;
        }
        let payload = &snapshot.latest.payload;
        let mut extra = Extra {
            kind: PayloadKind::Json,
            detail: String::new(),
            stats: None,
            peak: 0,
        };
        match view.mode {
            Mode::Frequency => {
                if snapshot.session_count == 0 {
                    idle += 1;
                    continue;
                }
            }
            Mode::InvalidJson => {
                let cached = view
                    .classes
                    .get(snapshot.path)
                    .filter(|(seq, _)| *seq == snapshot.latest.seq);
                let class = match cached {
                    Some((_, class)) => class.clone(),
                    None => {
                        let class = classify_payload(payload);
                        view.classes.insert(
                            snapshot.path.to_owned(),
                            (snapshot.latest.seq, class.clone()),
                        );
                        class
                    }
                };
                if class.kind == PayloadKind::Json
                    || view.kind_filter.is_some_and(|wanted| wanted != class.kind)
                {
                    continue;
                }
                extra.detail = detail_text(&class, payload);
                extra.kind = class.kind;
            }
            Mode::Quiet => {}
            Mode::Jitter => {
                let times = snapshot.history.iter().map(|message| message.received);
                let Some(stats) = interval_stats(times) else {
                    continue;
                };
                extra.stats = Some(stats);
            }
            Mode::Largest => {
                extra.peak = snapshot
                    .history
                    .iter()
                    .map(|message| message.payload.len())
                    .max()
                    .unwrap_or(0);
            }
            Mode::Retained => {
                if !snapshot.latest.retain {
                    continue;
                }
                extra.detail = payload_preview(payload);
            }
            Mode::Branches => {}
        }
        let mut row = Row::new(snapshot.path, snapshot.latest.received);
        row.count = snapshot.session_count;
        row.rate = window_rate(snapshot.window_len, snapshot.window_span);
        let gaps = snapshot.window_len.saturating_sub(1);
        row.interval = (gaps > 0).then(|| snapshot.window_span / gaps as u32);
        row.size = payload.len();
        row.peak = extra.peak;
        row.kind = extra.kind;
        row.detail = extra.detail;
        if let Some(stats) = extra.stats {
            row.interval = Some(stats.mean);
            row.min_interval = Some(stats.min);
            row.max_interval = Some(stats.max);
            row.jitter = Some(stats.jitter);
        }
        let silence = now.saturating_duration_since(snapshot.latest.received);
        row.overdue = row
            .interval
            .filter(|usual| !usual.is_zero())
            .map(|usual| silence.as_secs_f64() / usual.as_secs_f64());
        rows.push(row);
    }
    (rows, idle)
}

impl UiState {
    pub(super) fn open_analytics(&mut self) {
        self.analytics.invalidate();
        self.view = View::Analytics;
        self.refresh_analytics();
    }

    /// Recomputes the ranking when options changed or a second has passed.
    pub(super) fn refresh_analytics(&mut self) {
        let view = &self.analytics;
        let due = view.stale
            || (!view.paused
                && view
                    .computed_at
                    .is_none_or(|at| self.now.saturating_duration_since(at) >= REFRESH));
        if !due {
            return;
        }
        let view = &mut self.analytics;
        let (mut rows, idle) = collect_rows(view, &self.topics, self.now);
        let (sort, descending) = (view.sort, view.descending);
        rows.sort_by(|a, b| {
            let order = compare(a, b, sort);
            let order = if descending { order.reverse() } else { order };
            order.then_with(|| a.topic.cmp(&b.topic))
        });
        view.matching = rows.len();
        rows.truncate(MAX_ROWS);
        view.rows = rows;
        view.idle = idle;
        view.selection = view.selection.min(view.rows.len().saturating_sub(1));
        view.computed_at = Some(self.now);
        view.stale = false;
    }

    pub(super) fn handle_analytics_key(&mut self, code: KeyCode) -> UiAction {
        let page = self.analytics.viewport.saturating_sub(1).max(1);
        let last = self.analytics.rows.len().saturating_sub(1);
        let view = &mut self.analytics;
        if view.filtering {
            match code {
                KeyCode::Up => view.selection = view.selection.saturating_sub(1),
                KeyCode::Down => view.selection = (view.selection + 1).min(last),
                _ => view.edit_filter(code),
            }
            self.refresh_analytics();
            return UiAction::None;
        }
        match code {
            KeyCode::Esc if !view.filter.is_empty() => {
                view.filter.clear();
                view.restart_list();
            }
            KeyCode::Esc | KeyCode::Char('A') => self.view = View::Main,
            KeyCode::Char('/') => view.filtering = true,
            KeyCode::Char(' ') => view.toggle_pause(),
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Tab => view.set_mode(view.mode.step(true)),
            KeyCode::BackTab => view.set_mode(view.mode.step(false)),
            KeyCode::Char(digit @ '1'..='7') => {
                view.set_mode(Mode::ALL[digit as usize - '1' as usize]);
            }
            KeyCode::Char('s') => view.cycle_sort(),
            KeyCode::Char('r') => {
                view.descending = !view.descending;
                view.invalidate();
            }
            KeyCode::Char('f') if view.mode == Mode::InvalidJson => view.cycle_filter(),
            KeyCode::Char('d') if view.mode == Mode::Branches => view.cycle_depth(),
            KeyCode::Up | KeyCode::Char('k') => view.selection = view.selection.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selection = (view.selection + 1).min(last),
            KeyCode::PageUp => view.selection = view.selection.saturating_sub(page),
            KeyCode::PageDown => view.selection = (view.selection + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => view.selection = 0,
            KeyCode::End | KeyCode::Char('G') => view.selection = last,
            KeyCode::Enter => {
                if let Some(topic) = view.selected_topic().map(str::to_owned) {
                    self.main.reveal(&topic);
                    self.main.focus = Panel::Topics;
                    self.main.ensure_rows(&self.topics);
                    self.view = View::Main;
                }
            }
            KeyCode::Char('c' | 't') => {
                let topic = view.selected_topic().map(str::to_owned);
                self.copy(topic, "topic");
            }
            _ => {}
        }
        self.refresh_analytics();
        UiAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_group_by_the_first_levels_of_a_topic() {
        assert_eq!(branch_key("plant/line-1/status", 1), "plant");
        assert_eq!(branch_key("plant/line-1/status", 2), "plant/line-1");
        assert_eq!(branch_key("plant/line-1/status", 3), "plant/line-1/status");
        assert_eq!(branch_key("plant/line-1/status", 6), "plant/line-1/status");
        assert_eq!(branch_key("solo", 2), "solo");
    }
}
