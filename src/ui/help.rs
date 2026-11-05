//! The help overlay: tabs for shortcuts, views, concepts, changelog and about (plus a hidden snake game on `6`). Content and keys only; drawing is in `render`.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::KeyCode;

use super::{UiState, View};
use mate::snake::{Dir, Snake};

pub const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Views,
    Concepts,
    Changelog,
    About,
    /// Easter egg: not in `ALL`, reached with `6`.
    Snake,
}

impl HelpTab {
    pub const ALL: [HelpTab; 5] = [
        HelpTab::Keys,
        HelpTab::Views,
        HelpTab::Concepts,
        HelpTab::Changelog,
        HelpTab::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HelpTab::Keys => "Keys",
            HelpTab::Views => "Views",
            HelpTab::Concepts => "Concepts",
            HelpTab::Changelog => "Changelog",
            HelpTab::About => "About",
            HelpTab::Snake => "Snake",
        }
    }

    fn step(self, forward: bool) -> HelpTab {
        if self == HelpTab::Snake {
            return HelpTab::Keys;
        }
        let index = HelpTab::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or(0);
        let count = HelpTab::ALL.len();
        HelpTab::ALL[if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        }]
    }
}

pub enum Block {
    Section(&'static str),
    /// A key (or term) and its explanation.
    Entry(&'static str, &'static str),
    Text(&'static str),
}

use Block::{Entry, Section, Text};

const KEYS: &[Block] = &[
    Section("Global"),
    Entry("Tab / Shift+Tab", "Next / previous panel"),
    Entry("m", "Maximize or restore the focused panel"),
    Entry("h", "Show or hide the message history"),
    Entry(
        "< >  Ctrl+←/→",
        "Narrow / widen the focused panel (use < > if Ctrl+arrows fail)",
    ),
    Entry("o  x", "Connect the selected broker / disconnect"),
    Entry("L", "Logging: write received messages to .mqttlog files"),
    Entry(
        "R",
        "Replay: play recorded .mqttlog files to a connected broker",
    ),
    Entry(
        "p",
        "Publish a message (only if the broker allows it, see w); the dialog lists earlier messages to edit and send again",
    ),
    Entry("b", "Broker manager (add, edit, connect, ...)"),
    Entry("A", "Analytics: rankings over all topics"),
    Entry(
        "S",
        "Stream: all messages and connection events in arrival order",
    ),
    Entry("P", "Publish panel below the panes (p keeps the dialog)"),
    Entry(
        "Esc",
        "Close, clear filter, leave maximized view or history",
    ),
    Entry(
        "?  q",
        "Help / quit after a confirmation (Ctrl+Q asks too; Ctrl+Q twice quits)",
    ),
    Entry(
        "Ctrl+C  t",
        "Copy value or payload / copy topic (c copies the payload in the topic and history panels)",
    ),
    Section("Topic explorer"),
    Entry("↑↓ j k", "Move (PgUp/PgDn, g/G jump)"),
    Entry(
        "→ l  ←",
        "Expand / collapse (← on a leaf selects the parent)",
    ),
    Entry("Enter", "Expand branch, or inspect a leaf"),
    Entry(
        "+  -",
        "Expand all / collapse all topics (Ctrl+arrows resize the panes)",
    ),
    Entry(
        "Shift+← →",
        "Collapse / expand the selected branch with everything below it",
    ),
    Entry(
        "/",
        "Filter topics (case-insensitive, fuzzy); Enter keeps, Esc clears",
    ),
    Entry(
        "*  F",
        "Mark favorite (saved per broker) / show only favorites",
    ),
    Section("Payload inspector"),
    Entry("↑↓ j k  → l ←", "Move; expand / collapse nodes"),
    Entry("+  -", "Expand all / collapse all"),
    Entry("r", "Tree / raw (text / hex for non-JSON)"),
    Entry("c  C", "Copy value / copy whole payload"),
    Entry("t", "Copy the topic"),
    Entry(
        "i",
        "Metadata of the selected topic (QoS, size, intervals, MQTT 5 properties)",
    ),
    Entry("/  n  N", "Search; next / previous match"),
    Entry("0", "Reset the horizontal scroll (← → scroll sideways)"),
    Entry(
        "p",
        "Edit & publish: the shown message becomes the template of the publish editor (cursor at the start of the payload)",
    ),
    Entry("d", "Diff against the previous message"),
    Section("History"),
    Entry(
        "p",
        "Edit & publish: the selected message becomes the template of the publish editor (topic, payload, QoS, retain; cursor at the start of the payload)",
    ),
    Entry(
        "Ctrl+S",
        "Send the selected message again as received (needs publishing allowed)",
    ),
    Entry("↑↓ j k", "Select a message (newest first)"),
    Entry("Enter  l", "Inspect selected message / back to live"),
    Entry("d", "Mark a message, then compare with another"),
    Entry(
        "/",
        "Filter the messages by payload text or MQTT 5 property (origin=plc-7, content type, ...); Enter keeps, Esc clears",
    ),
];

const VIEWS: &[Block] = &[
    Section("Analytics"),
    Text("Press A in the explorer. Esc or A returns."),
    Entry("Tab  1-7", "Next list, or pick one directly"),
    Entry(
        "1  Frequency",
        "Topics by message count or rate since the last connection attempt; finds a PLC task that fires every cycle",
    ),
    Entry(
        "2  Not JSON",
        "Topics whose latest payload is not valid JSON; f filters by kind (broken, text, binary, empty)",
    ),
    Entry(
        "3  Quiet",
        "Silence since the last message and how overdue a topic is compared with its usual interval",
    ),
    Entry(
        "4  Jitter",
        "How irregular the message timing is (needs five or more messages in the history)",
    ),
    Entry(
        "5  Largest",
        "Latest payload size and the peak in the history",
    ),
    Entry(
        "6  Retained",
        "Topics whose latest message carried the retain flag",
    ),
    Entry(
        "7  Branches",
        "Totals per branch; d changes how many levels form one",
    ),
    Entry("s  r", "Sort by another column / reverse the order"),
    Entry(
        "/",
        "Filter the topics by name (fuzzy, like the explorer); Enter keeps, Esc clears",
    ),
    Entry(
        "Space",
        "Pause / resume: the list and its ages stop updating; changing a sort or filter still recalculates it",
    ),
    Entry("↑↓ j k", "Move (PgUp/PgDn, g/G jump)"),
    Entry("Enter", "Show the topic in the explorer"),
    Entry("t", "Copy the topic"),
    Section("Replay"),
    Text(
        "Press R in the explorer or the broker manager. Pick one or more .mqttlog files, then play them to any connected broker that allows publishing.",
    ),
    Entry(
        "Space",
        "Files: mark; player: start / pause (the first start asks for confirmation)",
    ),
    Entry(
        "Enter  p  a",
        "Files: load / type a folder or file / mark all",
    ),
    Entry("b", "Choose the target broker"),
    Entry("m  + -", "Recorded times or a fixed rate; slower / faster"),
    Entry("g  l", "Cap long pauses between messages / loop"),
    Entry("/  0 1 2", "Topic filter / QoS of the replayed messages"),
    Entry("r  Esc", "Restart / back to the file list"),
    Section("Stream"),
    Text(
        "Press S in the explorer. Esc or S returns. Every message and connection event of this run in arrival order, newest last; the last 50,000 entries (64 MiB of payload) are kept; C clears the list.",
    ),
    Entry(
        "↑↓ j k",
        "Move (PgUp/PgDn, g top, G or End follow the newest)",
    ),
    Entry(
        "Enter",
        "Expand / collapse the details: QoS, retain, size, time since the previous message, MQTT 5 properties and the formatted payload",
    ),
    Entry("+  -", "Expand / collapse the selected entry, like Enter"),
    Entry("Space", "Pause / resume; new entries wait while paused"),
    Entry("/", "Filter by topic (fuzzy); Enter keeps, Esc clears"),
    Entry("f", "Find text in payloads and MQTT 5 properties"),
    Entry(
        "R  e",
        "Only retained messages / show or hide connection events",
    ),
    Entry(
        "s  c  t",
        "Show in the explorer / copy payload / copy topic",
    ),
    Section("Broker manager"),
    Text("Press b in the explorer. Esc or b returns."),
    Entry("Enter  t", "Connect / test connection"),
    Entry(
        "Space",
        "Mark a broker; Enter then connects all marked brokers together",
    ),
    Entry("a e d v", "Add / edit / delete / certificates"),
    Entry("s", "Subscriptions of the selected broker"),
    Entry("u  *", "Duplicate / set or clear the default broker"),
    Entry("w", "Allow or forbid publishing for the broker"),
    Entry(
        "y",
        "Show or hide the broker's system topics ($SYS/#, which # never matches); off by default",
    ),
    Entry("x", "Disconnect"),
    Section("Broker editor"),
    Entry(
        "Enter",
        "Next field; save on the last one (a new name also renames the profile's folder)",
    ),
    Entry("Tab ↓  Shift+Tab ↑", "Next / previous field"),
    Entry("← →", "Choose the protocol (MQTT 3.1.1 or 5)"),
    Entry("Esc", "Cancel"),
    Section("Subscriptions"),
    Entry("Space", "Switch a filter on or off"),
    Entry("a  d", "Add / remove a topic filter"),
    Entry(
        "x",
        "Exclude topics: drop the messages matching a filter such as +/hmi/# although a subscription (e.g. #) delivers them",
    ),
    Entry("0 1 2", "QoS of the selected filter"),
    Entry("Esc", "Back to the broker manager"),
    Section("Certificates"),
    Entry("i  r", "Import a file / reference a file in place"),
    Entry("d", "Remove (with confirmation)"),
    Entry("1 2 3", "In the dialog: CA, client certificate, client key"),
    Entry(
        "← →  Home End",
        "In the path: move the cursor; Delete, Backspace and paste work there",
    ),
    Section("Publish dialog"),
    Entry("Enter", "Next field; review on the last one"),
    Entry(
        "Ctrl+S",
        "Send right away (the review still stops at mistakes)",
    ),
    Entry("Tab", "Next field; the last one leads to earlier messages"),
    Entry("← →  Space", "QoS and retain"),
    Entry(
        "Ctrl+J  Ctrl+F",
        "New line / switch a JSON payload between indented and one line",
    ),
    Entry("Ctrl+U", "Clear the field"),
    Entry(
        "Ctrl+V",
        "Paste the clipboard (also Shift+Insert); line breaks become new lines",
    ),
    Entry(
        "Ctrl+B",
        "Several brokers: send to the next connected broker that allows publishing",
    ),
    Entry(
        "Ctrl+O",
        "Load the payload from a UTF-8 text file (path, ~/ allowed; replaces the payload, one trailing line break is dropped)",
    ),
    Entry("Earlier messages", "↑↓ select, Enter loads, d forgets"),
    Entry("y  n", "In the review: send / go back"),
    Section("Publish panel"),
    Text(
        "P docks the same editor below the other panes, so topics and inspector stay visible while you write; p opens it instead of the dialog while it is shown. Typing goes to the panel while it has focus, so letters are not shortcuts there.",
    ),
    Entry(
        "P",
        "Show or hide the panel (outside it); the draft is kept while hidden",
    ),
    Entry(
        "Esc",
        "Leave the panel; Tab / Shift+Tab move between its fields",
    ),
    Entry(
        "Enter  Ctrl+S",
        "Review / send; the draft stays after sending",
    ),
    Entry(
        "Ctrl+O  Ctrl+F",
        "Load the payload from a file / switch JSON between indented and one line",
    ),
];

const CONCEPTS: &[Block] = &[
    Section("Reading the topic list"),
    Entry(
        "×N",
        "Messages the topic received since the last connection attempt; automatic reconnects do not reset it",
    ),
    Entry("● ○", "Updated within the last five seconds / earlier"),
    Entry("{…}  [12 items]", "Preview of a JSON object / array"),
    Entry("!", "The payload looks like JSON but is broken"),
    Section("Where a message comes from"),
    Text("The inspector title says which kind of message you are looking at:"),
    Entry("LIVE", "Received during the current connection"),
    Entry(
        "RETAINED",
        "Received during the current connection and flagged retained by the broker",
    ),
    Entry(
        "CACHED",
        "Kept from before the current connection until a new message replaces it",
    ),
    Entry("HISTORY", "An older message you pinned from the history"),
    Section("Connections"),
    Text(
        "Connecting to a different broker or set of brokers discards the previous data; reconnecting to the same ones keeps it.",
    ),
    Text(
        "With several brokers a digit marks each one: in the header, as a column in front of a topic (1· from broker 1, ·2 from broker 2, 12 from both) and in front of a stream entry.",
    ),
    Text(
        "MATE appends a random suffix to the client ID of every connection, so several instances can share one profile. A broker that only accepts a fixed ID needs it set with mate broker edit <id> --client-id <fixed-id>.",
    ),
    Section("Publishing is opt-in"),
    Text(
        "Profiles are read-only by default and the status bar says so. To allow publishing press b, select the broker, press w and connect again. The review step warns about retained messages, about an empty retained payload (which deletes the stored message) and about broken JSON.",
    ),
    Section("Limits"),
    Entry(
        "History",
        "100 messages and 512 KiB per topic; 128 MiB and 200,000 messages overall, oldest dropped first",
    ),
    Entry(
        "Previews",
        "Payloads above 64 KiB get no row preview ({…} or [...] by their first character)",
    ),
    Entry(
        "Dropped",
        "If the screen cannot keep up with a burst, messages are dropped and counted as a warning in the status bar",
    ),
];

pub fn blocks(tab: HelpTab) -> &'static [Block] {
    match tab {
        HelpTab::Keys => KEYS,
        HelpTab::Views => VIEWS,
        HelpTab::Concepts => CONCEPTS,
        HelpTab::Changelog | HelpTab::About | HelpTab::Snake => &[],
    }
}

#[derive(Debug)]
pub struct HelpState {
    pub tab: HelpTab,
    pub scroll: usize,
    /// Visible content rows and the largest scroll offset; both set by the renderer.
    pub viewport: usize,
    pub max_scroll: usize,
    /// A section title to scroll to when the overlay is next drawn.
    pub jump: Option<&'static str>,
    /// The snake game, created when its tab is first opened; it keeps its best score while MATE runs.
    pub snake: Option<Snake>,
    /// When the snake last moved; `None` while it stands still.
    snake_stepped: Option<Instant>,
}

impl Default for HelpState {
    fn default() -> Self {
        Self {
            tab: HelpTab::Keys,
            scroll: 0,
            viewport: 10,
            max_scroll: 0,
            jump: None,
            snake: None,
            snake_stepped: None,
        }
    }
}

impl HelpState {
    fn set_tab(&mut self, tab: HelpTab) {
        self.pause_snake();
        self.tab = tab;
        self.scroll = 0;
        self.jump = None;
        if tab == HelpTab::Snake && self.snake.is_none() {
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(1, |elapsed| elapsed.as_nanos() as u64);
            self.snake = Some(Snake::new(seed));
        }
    }

    /// A game must not run on behind another tab or a closed overlay.
    fn pause_snake(&mut self) {
        if let Some(snake) = self.snake.as_mut().filter(|snake| snake.is_running()) {
            snake.toggle();
        }
        self.snake_stepped = None;
    }
}

impl UiState {
    /// Opens the help on the tab and section that describe the current view.
    pub(super) fn open_help(&mut self) {
        let (tab, jump) = match self.view {
            View::Main => (HelpTab::Keys, None),
            View::Analytics => (HelpTab::Views, Some("Analytics")),
            View::Stream => (HelpTab::Views, Some("Stream")),
            View::Replay => (HelpTab::Views, Some("Replay")),
            View::Brokers => (HelpTab::Views, Some("Broker manager")),
            View::Subscriptions => (HelpTab::Views, Some("Subscriptions")),
            View::Certificates => (HelpTab::Views, Some("Certificates")),
        };
        self.help.set_tab(tab);
        self.help.jump = jump;
        self.help_open = true;
    }

    /// Views where `?` is not typed into a field.
    pub(super) fn can_open_help(&self) -> bool {
        match self.view {
            View::Main => {
                !self.main.search.active
                    && !self.main.inspector.search.active
                    && !self.main.history_search.active
            }
            View::Analytics => !self.analytics.filtering,
            View::Stream => self.stream_view.typing.is_none(),
            View::Replay => !self.replay.typing(),
            View::Brokers => true,
            View::Subscriptions | View::Certificates => false,
        }
    }

    /// Moves the snake when it is due; called from the UI clock.
    pub(super) fn advance_snake(&mut self, now: Instant) {
        let help = &mut self.help;
        let Some(snake) = help
            .snake
            .as_mut()
            .filter(|snake| self.help_open && help.tab == HelpTab::Snake && snake.is_running())
        else {
            help.snake_stepped = None;
            return;
        };
        let last = *help.snake_stepped.get_or_insert(now);
        if now.duration_since(last) >= snake.interval() {
            snake.step();
            help.snake_stepped = Some(now);
            self.dirty = true;
        }
    }

    fn handle_snake_key(&mut self, code: KeyCode) -> bool {
        let Some(snake) = self.help.snake.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Up => snake.turn(Dir::Up),
            KeyCode::Down => snake.turn(Dir::Down),
            KeyCode::Left => snake.turn(Dir::Left),
            KeyCode::Right => snake.turn(Dir::Right),
            KeyCode::Enter | KeyCode::Char(' ') => snake.toggle(),
            _ => return false,
        }
        self.help.snake_stepped = self
            .help
            .snake
            .as_ref()
            .and_then(|snake| snake.is_running().then_some(self.now));
        true
    }

    pub(super) fn handle_help_key(&mut self, code: KeyCode) {
        if self.help.tab == HelpTab::Snake && self.handle_snake_key(code) {
            return;
        }
        let help = &mut self.help;
        let page = help.viewport.saturating_sub(1).max(1);
        match code {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => {
                help.pause_snake();
                self.help_open = false;
            }
            KeyCode::Char('6') => help.set_tab(HelpTab::Snake),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                help.set_tab(help.tab.step(true));
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                help.set_tab(help.tab.step(false));
            }
            KeyCode::Char(digit @ '1'..='5') => {
                help.set_tab(HelpTab::ALL[digit as usize - '1' as usize]);
            }
            KeyCode::Up | KeyCode::Char('k') => help.scroll = help.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => help.scroll = help.scroll.saturating_add(1),
            KeyCode::PageUp => help.scroll = help.scroll.saturating_sub(page),
            KeyCode::PageDown => help.scroll = help.scroll.saturating_add(page),
            KeyCode::Home | KeyCode::Char('g') => help.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => help.scroll = usize::MAX,
            _ => {}
        }
    }
}
