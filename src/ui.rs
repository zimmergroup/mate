//! Terminal UI: application state, input handling and the broker/certificate dialogs.
//! Rendering lives in `render`; the main view's state and keys in `main_view`.

mod analytics;
mod help;
mod info;
mod inspector;
mod layout;
mod logging;
mod main_view;
mod publish;
mod render;
mod replay;
mod stream;
mod subscriptions;
mod theme;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mate::{
    broker::{BrokerStore, unique_broker_id},
    certificates::{
        CertificateKind, CertificateMetadata, CertificateService, validate_tls_profile,
    },
    config::{AppConfig, AuthConfig, BrokerConfig, SubscriptionConfig},
    mqtt::{self, BrokerEvent, ConnectionState, PublishRequest},
    mqttlog::LogFormat,
    security::store_broker_password,
    stats::RateMeter,
    stream::{StreamLog, StreamMessage},
    topics::{Message, MessageMeta, TopicStore, valid_topic_filter},
    ui_state::{LogSettings, PublishHistoryEntry, UiSettings},
};

pub use render::render;

use analytics::AnalyticsView;
use help::HelpState;
use logging::LogDialog;
use main_view::MainView;
use replay::ReplayView;
use stream::StreamView;
pub use subscriptions::SubscriptionChange;
use subscriptions::SubscriptionsView;

const FIELD_NAME: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_PORT: usize = 2;
const FIELD_PROTOCOL: usize = 3;
/// Only used when adding a broker; existing subscriptions are managed in their own dialog.
const FIELD_SUBSCRIPTION: usize = 4;
const FIELD_USERNAME: usize = 5;
const FIELD_PASSWORD: usize = 6;
const EDITOR_LAST_FIELD: usize = FIELD_PASSWORD;
const NOTICE_LIFETIME: Duration = Duration::from_secs(6);
const TICK_REDRAW_INTERVAL: Duration = Duration::from_millis(500);

struct BrokerEditor {
    editing_id: Option<String>,
    name: String,
    host: String,
    port: String,
    protocol: String,
    subscription: String,
    username: String,
    password: String,
    field: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Brokers,
    Subscriptions,
    Certificates,
    Analytics,
    Stream,
    Replay,
}

#[derive(Debug, Clone, Copy)]
enum CertificateEditMode {
    Import,
    Reference,
}

struct CertificateEditor {
    mode: CertificateEditMode,
    kind: CertificateKind,
    path: String,
    /// Cursor in the path as a character index.
    cursor: usize,
    field: usize,
    replace: bool,
    error: Option<String>,
}

enum Confirmation {
    RemoveBroker(String),
    RemoveCertificate(CertificateKind),
    Quit,
}

struct Notice {
    text: String,
    at: Instant,
    error: bool,
}

/// Text the event loop should put on the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRequest {
    pub text: String,
    pub what: String,
}

/// The single authoritative record of the broker connection shown in the UI.
#[derive(Debug, Clone)]
struct ConnectionInfo {
    broker_id: String,
    /// Shared with the messages this connection delivers.
    broker_arc: Arc<str>,
    broker_name: String,
    state: ConnectionState,
    detail: Option<String>,
    tls: bool,
    mutual_tls: bool,
    /// Whether the profile allows publishing; decided when the connection started.
    allow_publish: bool,
    protocol: &'static str,
    /// Messages with a larger sequence number arrived during the current connection.
    connected_after_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TlsStatus {
    Off,
    Valid { mutual: bool },
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TestOutcome {
    Running,
    Passed,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSource {
    Live,
    Retained,
    Cached,
}

pub struct UiState {
    pub config: AppConfig,
    pub broker_store: BrokerStore,
    pub selected_broker: Option<String>,
    pub topics: TopicStore,
    pub dropped_messages: u64,
    /// Brokers whose messages are held in `topics`.
    topics_brokers: BTreeSet<String>,
    /// Broker IDs in the order of their markers (slot 1, 2, ...), as long as `topics` lives.
    broker_slots: Vec<String>,
    /// The connections of the current run, in the order they were started.
    connections: Vec<ConnectionInfo>,
    /// Brokers marked in the broker manager; `Enter` connects all of them.
    marked_brokers: BTreeSet<String>,
    rate: RateMeter,
    now: Instant,
    last_tick_redraw: Instant,
    dirty: bool,
    help_open: bool,
    help: HelpState,
    notice: Option<Notice>,
    pending_clipboard: Option<ClipboardRequest>,
    /// Ctrl+V was pressed: the event loop reads the system clipboard and pastes it.
    paste_requested: bool,
    main: MainView,
    analytics: AnalyticsView,
    /// Every message and connection event of this run, newest last.
    stream: StreamLog,
    stream_view: StreamView,
    editor: Option<BrokerEditor>,
    certificate_editor: Option<CertificateEditor>,
    /// Whether and where `.mqttlog` files are written.
    log: LogSettings,
    log_format: LogFormat,
    replay: ReplayView,
    log_dialog: Option<LogDialog>,
    confirmation: Option<Confirmation>,
    publish_editor: Option<publish::PublishEditor>,
    /// The panel's draft while the panel is hidden.
    publish_draft: Option<publish::PublishEditor>,
    subscriptions: Option<SubscriptionsView>,
    view: View,
    broker_selection: usize,
    certificate_selection: usize,
    certificates: Vec<CertificateMetadata>,
    broker_tls: HashMap<String, TlsStatus>,
    test_results: HashMap<String, TestOutcome>,
    /// What `ui-state.toml` currently contains.
    persisted: UiSettings,
    /// Messages published per broker, newest first; saved with the other UI settings.
    publish_history: BTreeMap<String, Vec<PublishHistoryEntry>>,
    persist_failed: bool,
}

pub enum UiAction {
    None,
    Connect(BrokerConfig),
    /// Replaces the running connections by one to each of these brokers.
    ConnectMany(Vec<BrokerConfig>),
    Disconnect,
    TestBroker(BrokerConfig),
    /// A message for the broker with this ID.
    Publish(String, PublishRequest),
    /// A live subscription change for the broker with this ID.
    Subscription(String, SubscriptionChange),
    Quit,
}

impl UiState {
    pub fn new(
        config: AppConfig,
        broker_store: BrokerStore,
        selected_broker: Option<String>,
    ) -> Self {
        let now = Instant::now();
        let mut state = Self {
            config,
            broker_store,
            selected_broker,
            topics: TopicStore::default(),
            dropped_messages: 0,
            topics_brokers: BTreeSet::new(),
            broker_slots: Vec::new(),
            connections: Vec::new(),
            marked_brokers: BTreeSet::new(),
            rate: RateMeter::new(now),
            now,
            last_tick_redraw: now,
            dirty: true,
            help_open: false,
            help: HelpState::default(),
            notice: None,
            pending_clipboard: None,
            paste_requested: false,
            main: MainView::default(),
            analytics: AnalyticsView::default(),
            stream: StreamLog::default(),
            stream_view: StreamView::default(),
            editor: None,
            certificate_editor: None,
            log: LogSettings::default(),
            log_format: LogFormat::default(),
            replay: ReplayView::default(),
            log_dialog: None,
            confirmation: None,
            publish_editor: None,
            publish_draft: None,
            subscriptions: None,
            view: View::Main,
            broker_selection: 0,
            certificate_selection: 0,
            certificates: Vec::new(),
            broker_tls: HashMap::new(),
            test_results: HashMap::new(),
            persisted: UiSettings::default(),
            publish_history: BTreeMap::new(),
            persist_failed: false,
        };
        match UiSettings::load(state.broker_store.root()) {
            Ok(saved) => {
                if let Some(layout) = &saved.layout {
                    state.main.layout.apply(layout);
                }
                state.publish_history = saved.publish_history.clone();
                state.log = saved.logging.clone();
                state.persisted = saved;
            }
            Err(error) => state.notify(format!("{error:#}"), true),
        }
        match LogFormat::new(&state.config.log_format) {
            Ok(format) => state.log_format = format,
            Err(error) => state.notify(format!("{error:#}; the default log format is used"), true),
        }
        state.refresh_broker_details();
        state
    }

    /// How `.mqttlog` lines are written and read, from `[log_format]` in `config.toml`.
    pub fn log_format(&self) -> &LogFormat {
        &self.log_format
    }

    /// Advances the UI clock; call once per event-loop iteration.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        self.advance_snake(now);
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| now.duration_since(notice.at) >= NOTICE_LIFETIME)
        {
            self.notice = None;
            self.dirty = true;
        }
        let time_dependent = !self.connections.is_empty()
            || self.replay.is_playing()
            || self.topics.topic_count() > 0
            || self.notice.is_some();
        if time_dependent && now.duration_since(self.last_tick_redraw) >= TICK_REDRAW_INTERVAL {
            self.dirty = true;
        }
    }

    /// True once per change; the event loop redraws only when this returns true.
    pub fn take_redraw(&mut self) -> bool {
        let dirty = std::mem::take(&mut self.dirty);
        if dirty {
            self.last_tick_redraw = self.now;
        }
        dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        self.pending_clipboard.take()
    }

    /// True once after Ctrl+V (or Shift+Insert) in a text field.
    pub fn take_paste_request(&mut self) -> bool {
        std::mem::take(&mut self.paste_requested)
    }

    /// A one-line field has the keyboard: pasted text is typed into it character by character.
    fn single_line_input_active(&self) -> bool {
        if self.help_open || self.confirmation.is_some() {
            return false;
        }
        if let Some(dialog) = &self.log_dialog {
            return dialog.field != logging::FIELD_ENABLED;
        }
        match self.view {
            View::Main => {
                self.main.search.active
                    || self.main.inspector.search.active
                    || self.main.history_search.active
            }
            View::Stream => self.stream_view.typing.is_some(),
            View::Analytics => self.analytics.filtering,
            View::Replay => self.replay.typing(),
            _ => false,
        }
    }

    /// Shows the outcome of an action that ran outside the UI (clipboard, live subscription).
    pub fn show_notice(&mut self, text: impl Into<String>, error: bool) {
        self.notify(text, error);
        self.dirty = true;
    }

    /// Whether this run has a connection to `broker_id`, in any state.
    pub fn has_connection(&self, broker_id: &str) -> bool {
        self.connection_of(broker_id).is_some()
    }

    fn connection_of(&self, broker_id: &str) -> Option<&ConnectionInfo> {
        self.connections
            .iter()
            .find(|connection| connection.broker_id == broker_id)
    }

    fn any_connected(&self) -> bool {
        self.connections
            .iter()
            .any(|connection| connection.state == ConnectionState::Connected)
    }

    /// The profile name of a broker, or its ID when the profile is gone.
    fn broker_name(&self, broker_id: &str) -> String {
        self.find_profile(broker_id)
            .map_or_else(|| broker_id.to_owned(), |profile| profile.name.clone())
    }

    /// The marker number of a broker; only shown while topics of several brokers are mixed.
    fn broker_badge(&self, broker_id: &str) -> Option<usize> {
        if self.broker_slots.len() < 2 {
            return None;
        }
        self.broker_slots
            .iter()
            .position(|known| known == broker_id)
            .map(|index| index + 1)
    }

    /// Key of the favorites of the current set of brokers in `ui-state.toml`.
    fn favorites_key(&self) -> String {
        self.topics_brokers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("+")
    }

    /// Forgets the topics, history and stream of the brokers shown so far.
    fn clear_session_data(&mut self) {
        self.topics.clear();
        self.main.reset_for_new_data();
        self.analytics.reset();
        self.stream.clear();
        self.stream_view.reset();
        self.topics_brokers.clear();
        self.broker_slots.clear();
        self.connections.clear();
    }

    /// Why a profile cannot be renamed right now: its folder would move under a running connection.
    fn rename_blocker(&self, id: &str, new_name: &str) -> Option<String> {
        let running = self.connection_of(id).is_some_and(|connection| {
            !matches!(
                connection.state,
                ConnectionState::Disconnected | ConnectionState::Error
            )
        });
        if !running {
            return None;
        }
        let others = self
            .config
            .brokers
            .iter()
            .filter(|profile| profile.id != id)
            .map(|profile| profile.id.as_str());
        (unique_broker_id(new_name, others) != id).then(|| {
            "Disconnect first (x): a new name also renames the profile's folder".to_owned()
        })
    }

    /// Repairs what refers to a broker by ID after its folder was renamed.
    fn apply_broker_rename(&mut self, old: &str, new: &str) {
        if self.connection_of(old).is_some() || self.topics_brokers.contains(old) {
            self.clear_session_data();
        }
        if self.config.general.default_broker.as_deref() == Some(old) {
            self.config.general.default_broker = Some(new.to_owned());
        }
        if let Some(outcome) = self.test_results.remove(old) {
            self.test_results.insert(new.to_owned(), outcome);
        }
        if self.marked_brokers.remove(old) {
            self.marked_brokers.insert(new.to_owned());
        }
        if self.replay.target.as_deref() == Some(old) {
            self.replay.target = None;
        }
        if self.selected_broker.as_deref() == Some(old) {
            self.selected_broker = Some(new.to_owned());
        }
        if let Some(history) = self.publish_history.remove(old) {
            self.publish_history.insert(new.to_owned(), history);
        }
        self.persisted.rename_broker(old, new);
        self.config.brokers.retain(|profile| profile.id != old);
    }

    /// A single connection; see `begin_connections`.
    #[cfg(test)]
    pub fn begin_connection(&mut self, profile: &BrokerConfig) {
        self.begin_connections(std::slice::from_ref(profile));
    }

    /// Starts a new set of connections; topics stay only when the same brokers were shown before.
    pub fn begin_connections(&mut self, profiles: &[BrokerConfig]) {
        let ids: BTreeSet<String> = profiles.iter().map(|profile| profile.id.clone()).collect();
        if self.topics_brokers != ids {
            self.clear_session_data();
            self.topics_brokers = ids;
            self.broker_slots = profiles.iter().map(|profile| profile.id.clone()).collect();
            self.main.favorites = self
                .persisted
                .favorites
                .get(&self.favorites_key())
                .cloned()
                .unwrap_or_default();
        }
        self.topics.begin_session();
        self.analytics.invalidate();
        self.dropped_messages = 0;
        self.rate = RateMeter::new(self.now);
        self.selected_broker = profiles.first().map(|profile| profile.id.clone());
        self.connections = profiles
            .iter()
            .map(|profile| ConnectionInfo {
                broker_id: profile.id.clone(),
                broker_arc: Arc::from(profile.id.as_str()),
                broker_name: profile.name.clone(),
                state: ConnectionState::Connecting,
                detail: None,
                tls: profile.tls.enabled,
                mutual_tls: profile.tls.enabled && profile.tls.client_cert.is_some(),
                allow_publish: profile.allow_publish,
                protocol: protocol_label(&profile.protocol),
                connected_after_seq: None,
            })
            .collect();
        for profile in profiles {
            self.stream.push_event(
                SystemTime::now(),
                format!("Connecting to {}", profile.name),
                false,
            );
        }
        self.dirty = true;
    }

    /// The terminal title: broker first, so a taskbar shows it before cutting the rest off.
    pub fn window_title(&self) -> String {
        let title = match self.connections.as_slice() {
            [] => self
                .selected_broker
                .as_deref()
                .and_then(|wanted| self.find_profile(wanted))
                .map_or_else(
                    || "MATE".to_owned(),
                    |profile| format!("{} \u{b7} MATE", profile.name),
                ),
            [connection] => format!(
                "{} \u{2013} {} \u{b7} MATE",
                connection.broker_name,
                theme::state_label(connection.state)
            ),
            many => {
                let names: Vec<&str> = many
                    .iter()
                    .map(|connection| connection.broker_name.as_str())
                    .collect();
                let connected = many
                    .iter()
                    .filter(|connection| connection.state == ConnectionState::Connected)
                    .count();
                format!(
                    "{} \u{2013} {connected}/{} connected \u{b7} MATE",
                    names.join(", "),
                    many.len()
                )
            }
        };
        // Broker names come from profiles, which may be imported; no control characters in an escape sequence.
        title
            .chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect()
    }

    pub fn mark_disconnected(&mut self) {
        for connection in &mut self.connections {
            connection.state = ConnectionState::Disconnected;
            connection.detail = None;
            connection.connected_after_seq = None;
        }
        self.dirty = true;
    }

    /// Applies an event of the first (or only) connection.
    #[cfg(test)]
    pub fn apply_broker_event(&mut self, event: BrokerEvent) {
        let id = self
            .connections
            .first()
            .map(|connection| connection.broker_id.clone())
            .unwrap_or_default();
        self.apply_event_from(&id, event);
    }

    /// Prefixes the broker's name while several brokers are connected.
    fn with_broker_name(&self, broker_id: &str, text: String) -> String {
        match self.connection_of(broker_id) {
            Some(connection) if self.connections.len() > 1 => {
                format!("{}: {text}", connection.broker_name)
            }
            _ => text,
        }
    }

    pub fn apply_event_from(&mut self, broker_id: &str, event: BrokerEvent) {
        self.dirty = true;
        match event {
            BrokerEvent::State { state, detail } => {
                let last_seq = self.topics.last_seq();
                let changed = self
                    .connection_of(broker_id)
                    .is_some_and(|connection| connection.state != state);
                if changed {
                    let text = match &detail {
                        Some(detail) => format!("{}: {detail}", theme::state_label(state)),
                        None => theme::state_label(state).to_owned(),
                    };
                    let text = self.with_broker_name(broker_id, text);
                    self.stream.push_event(
                        SystemTime::now(),
                        text,
                        state == ConnectionState::Error,
                    );
                }
                if let Some(connection) = self
                    .connections
                    .iter_mut()
                    .find(|connection| connection.broker_id == broker_id)
                {
                    connection.state = state;
                    connection.detail = detail;
                    if state == ConnectionState::Connected {
                        connection.connected_after_seq = Some(last_seq);
                    }
                }
            }
            BrokerEvent::Notice(text) => {
                let text = self.with_broker_name(broker_id, text);
                self.stream
                    .push_event(SystemTime::now(), text.clone(), false);
                self.notify(text, false);
            }
            BrokerEvent::Failed(message) => {
                let text = self.with_broker_name(broker_id, message.clone());
                self.stream
                    .push_event(SystemTime::now(), text.clone(), true);
                if let Some(connection) = self
                    .connections
                    .iter_mut()
                    .find(|connection| connection.broker_id == broker_id)
                {
                    connection.state = ConnectionState::Error;
                    connection.detail = Some(message);
                }
                self.notify(text, true);
            }
            BrokerEvent::Message(message) => {
                let Some(broker) = self
                    .connection_of(broker_id)
                    .map(|connection| Arc::clone(&connection.broker_arc))
                else {
                    return;
                };
                self.rate.record(message.received);
                self.topics.insert_message(
                    &message.topic,
                    message.payload,
                    MessageMeta {
                        qos: message.qos,
                        retain: message.retain,
                        dup: message.dup,
                        received: message.received,
                        wall: message.wall,
                        properties: message.properties,
                        broker: Some(Arc::clone(&broker)),
                    },
                );
                if let Some(latest) = self.topics.latest_message(&message.topic) {
                    let since_previous = self
                        .topics
                        .previous_message(&message.topic, latest.seq)
                        .map(|previous| {
                            latest.received.saturating_duration_since(previous.received)
                        });
                    self.stream.push_message(
                        latest.wall,
                        StreamMessage {
                            topic: message.topic.clone(),
                            payload: latest.payload.clone(),
                            qos: latest.qos,
                            retain: latest.retain,
                            dup: latest.dup,
                            properties: latest.properties.clone(),
                            since_previous,
                            broker: Some(broker),
                        },
                    );
                }
            }
        }
    }

    pub fn apply_test_result(&mut self, broker_id: &str, result: Result<(), String>) {
        let outcome = match result {
            Ok(()) => TestOutcome::Passed,
            Err(error) => TestOutcome::Failed(error),
        };
        self.test_results.insert(broker_id.to_owned(), outcome);
        self.dirty = true;
    }

    /// Where a message came from relative to the current connection.
    fn data_source(&self, message: &Message) -> DataSource {
        let connection = match &message.broker {
            Some(broker) => self.connection_of(broker),
            None => self.connections.first(),
        };
        match connection {
            Some(connection)
                if connection.state == ConnectionState::Connected
                    && connection
                        .connected_after_seq
                        .is_some_and(|seq| message.seq > seq) =>
            {
                if message.retain {
                    DataSource::Retained
                } else {
                    DataSource::Live
                }
            }
            _ => DataSource::Cached,
        }
    }

    fn find_profile(&self, id_or_name: &str) -> Option<&BrokerConfig> {
        self.config
            .brokers
            .iter()
            .find(|broker| broker.id == id_or_name || broker.name == id_or_name)
    }

    /// The profile `o` connects: the selected one, else the default, else the only one.
    fn resolve_profile(&self) -> Option<BrokerConfig> {
        let wanted = self
            .selected_broker
            .clone()
            .or_else(|| self.config.general.default_broker.clone());
        wanted
            .as_deref()
            .and_then(|wanted| self.find_profile(wanted))
            .or_else(|| (self.config.brokers.len() == 1).then(|| &self.config.brokers[0]))
            .cloned()
    }

    /// Marks the profiles named on the command line (by ID or name) for a connection to all of them.
    pub fn mark_brokers(&mut self, wanted: &[String]) {
        if wanted.len() < 2 {
            return;
        }
        let ids: Vec<String> = wanted
            .iter()
            .filter_map(|name| self.find_profile(name).map(|profile| profile.id.clone()))
            .collect();
        self.marked_brokers.extend(ids);
    }

    /// What to do right after starting: connect with `--connect`, or when the profile asks for it.
    pub fn startup_connect(&mut self, force: bool) -> UiAction {
        if force && self.marked_brokers.len() > 1 {
            let profiles: Vec<BrokerConfig> = self
                .config
                .brokers
                .iter()
                .filter(|profile| self.marked_brokers.contains(&profile.id))
                .cloned()
                .collect();
            return UiAction::ConnectMany(profiles);
        }
        if force {
            return self.connect_selected();
        }
        match self.resolve_profile() {
            Some(profile) if profile.auto_connect => UiAction::Connect(profile),
            _ => UiAction::None,
        }
    }

    fn connect_selected(&mut self) -> UiAction {
        match self.resolve_profile() {
            Some(profile) => UiAction::Connect(profile),
            None if self.config.brokers.is_empty() => {
                self.notify("No broker profiles yet; press b, then a, to add one", true);
                UiAction::None
            }
            None => {
                self.notify("Select a broker and press Enter to connect", false);
                self.refresh_broker_details();
                self.view = View::Brokers;
                UiAction::None
            }
        }
    }

    fn refresh_broker_details(&mut self) {
        self.broker_tls = self
            .config
            .brokers
            .iter()
            .map(|profile| {
                let status = if !profile.tls.enabled {
                    TlsStatus::Off
                } else {
                    match validate_tls_profile(profile) {
                        Ok(()) => TlsStatus::Valid {
                            mutual: profile.tls.client_cert.is_some(),
                        },
                        Err(error) => TlsStatus::Invalid(format!("{error:#}")),
                    }
                };
                (profile.id.clone(), status)
            })
            .collect();
    }
}

pub(crate) fn protocol_label(protocol: &str) -> &'static str {
    match protocol {
        "mqtt311" => "MQTT 3.1.1",
        "mqtt5" => "MQTT 5",
        _ => "MQTT",
    }
}

#[cfg(test)]
pub fn handle_key(state: &mut UiState, key: KeyCode) -> UiAction {
    handle_key_event(state, KeyEvent::new(key, KeyModifiers::NONE))
}

pub fn handle_key_event(state: &mut UiState, key: KeyEvent) -> UiAction {
    let was_asking = matches!(state.confirmation, Some(Confirmation::Quit));
    let action = dispatch_key_event(state, key);
    state.persist_ui_settings();
    if matches!(action, UiAction::Quit) && !was_asking {
        state.confirmation = Some(Confirmation::Quit);
        return UiAction::None;
    }
    action
}

/// Inserts pasted text (bracketed paste) into the field that has the focus.
pub fn handle_paste(state: &mut UiState, text: &str) {
    state.dirty = true;
    let one_line = text.replace(['\r', '\n'], " ");
    let publish_typing = state.publish_typing();
    if let Some(editor) = state.publish_editor.as_mut().filter(|_| publish_typing) {
        if let Some(path) = editor.file_prompt.as_mut() {
            path.push_str(one_line.trim());
        } else {
            match editor.field {
                publish::FIELD_TOPIC => editor.insert_topic(one_line.trim()),
                publish::FIELD_PAYLOAD => {
                    editor.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
                _ => {}
            }
        }
        editor.error = None;
    } else if let Some(editor) = state
        .subscriptions
        .as_mut()
        .and_then(|view| view.editor.as_mut())
    {
        if editor.field == subscriptions::FIELD_TOPIC {
            editor.topic.push_str(one_line.trim());
        }
        editor.error = None;
    } else if let Some(editor) = state.editor.as_mut() {
        match editor.field {
            FIELD_NAME => editor.name.push_str(&one_line),
            FIELD_HOST => editor.host.push_str(one_line.trim()),
            FIELD_PORT => editor
                .port
                .extend(one_line.chars().filter(char::is_ascii_digit)),
            FIELD_SUBSCRIPTION => editor.subscription.push_str(one_line.trim()),
            FIELD_USERNAME => editor.username.push_str(one_line.trim()),
            FIELD_PASSWORD => editor
                .password
                .push_str(text.trim_end_matches(['\r', '\n'])),
            _ => {}
        }
        editor.error = None;
    } else if let Some(editor) = state.certificate_editor.as_mut()
        && editor.field == 1
    {
        publish::insert_at(&mut editor.path, &mut editor.cursor, one_line.trim());
        editor.error = None;
    } else if state.single_line_input_active() {
        for character in one_line.trim().chars().filter(|c| !c.is_control()) {
            dispatch_key_event(
                state,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }
    }
}

fn dispatch_key_event(state: &mut UiState, key: KeyEvent) -> UiAction {
    state.dirty = true;
    if matches!(state.confirmation, Some(Confirmation::Quit)) {
        // Asking to quit takes every key, also while a dialog is open behind it.
        let again = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q');
        return match key.code {
            KeyCode::Char('y') | KeyCode::Enter => UiAction::Quit,
            _ if again => UiAction::Quit,
            KeyCode::Char('n') | KeyCode::Esc => {
                state.confirmation = None;
                UiAction::None
            }
            _ => UiAction::None,
        };
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q') => return UiAction::Quit,
            // Terminals that do not intercept Ctrl+V deliver it as a key; the event loop reads the clipboard.
            KeyCode::Char('v') => {
                state.paste_requested = true;
                return UiAction::None;
            }
            // Ctrl+C copies instead of quitting; text fields and dialogs ignore it.
            KeyCode::Char('c') => {
                if state.is_plain_main_view() {
                    state.copy_in_context();
                }
                return UiAction::None;
            }
            KeyCode::Char('s') if state.is_plain_main_view() => {
                return state.republish_from_history();
            }
            KeyCode::Left | KeyCode::Right if state.is_plain_main_view() => {
                state.resize_focused(if key.code == KeyCode::Right { 1 } else { -1 });
                return UiAction::None;
            }
            _ => {}
        }
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Insert {
        state.paste_requested = true;
        return UiAction::None;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Left | KeyCode::Right)
        && state.is_plain_main_view()
        && state.main.focus == layout::Panel::Topics
    {
        state.topic_subtree(key.code == KeyCode::Right);
        return UiAction::None;
    }
    let code = key.code;
    if state.publish_typing() {
        return state.dispatch_publish_key(key);
    }
    if state.editor.is_some() {
        handle_editor_key(state, code);
        return UiAction::None;
    }
    if state.certificate_editor.is_some() {
        handle_certificate_editor_key(state, code);
        return UiAction::None;
    }
    if state.log_dialog.is_some() {
        state.handle_log_dialog_key(code);
        return UiAction::None;
    }
    if state.confirmation.is_some() {
        handle_confirmation_key(state, code);
        return UiAction::None;
    }
    if state.help_open {
        state.handle_help_key(code);
        return UiAction::None;
    }
    if code == KeyCode::Char('?') && state.can_open_help() {
        state.open_help();
        return UiAction::None;
    }
    if code == KeyCode::Char('L') && state.can_open_help() {
        state.open_log_dialog();
        return UiAction::None;
    }
    match state.view {
        View::Brokers => handle_broker_manager_key(state, code),
        View::Subscriptions => state.handle_subscriptions_key(code),
        View::Certificates => {
            handle_certificate_manager_key(state, code);
            UiAction::None
        }
        View::Analytics => state.handle_analytics_key(code),
        View::Stream => state.handle_stream_key(code),
        View::Replay => state.handle_replay_key(code),
        View::Main => state.handle_main_key(code),
    }
}

impl UiState {
    /// Writes layout and favorites when they differ from the saved file; stops trying after a failure.
    fn persist_ui_settings(&mut self) {
        if self.persist_failed {
            return;
        }
        let mut wanted = self.persisted.clone();
        let layout = self.main.layout.settings();
        let saved_layout = self
            .persisted
            .layout
            .clone()
            .unwrap_or_else(|| layout::LayoutPrefs::default().settings());
        if layout != saved_layout {
            wanted.layout = Some(layout);
        }
        if !self.topics_brokers.is_empty() {
            let key = self.favorites_key();
            if self.main.favorites.is_empty() {
                wanted.favorites.remove(&key);
            } else {
                wanted.favorites.insert(key, self.main.favorites.clone());
            }
        }
        wanted.logging = self.log.clone();
        wanted.publish_history = self
            .publish_history
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(broker, list)| (broker.clone(), list.clone()))
            .collect();
        if wanted == self.persisted {
            return;
        }
        // Another MATE instance may have saved since this one loaded; keep what it changed.
        let root = self.broker_store.root();
        let disk = UiSettings::load(root).unwrap_or_else(|_| wanted.clone());
        let merged = UiSettings::merge_changes(&self.persisted, &wanted, disk);
        match merged.save(root) {
            Ok(()) => self.persisted = wanted,
            Err(error) => {
                self.persist_failed = true;
                self.notify(format!("Settings are not saved: {error:#}"), true);
            }
        }
    }

    fn is_plain_main_view(&self) -> bool {
        self.view == View::Main
            && self.editor.is_none()
            && self.certificate_editor.is_none()
            && self.log_dialog.is_none()
            && self.confirmation.is_none()
            && !self.publish_typing()
            && !self.help_open
            && !self.main.search.active
            && !self.main.inspector.search.active
            && !self.main.history_search.active
    }

    fn broker_saved(&mut self, profile: BrokerConfig) {
        self.selected_broker = Some(profile.id.clone());
        if let Some(existing) = self
            .config
            .brokers
            .iter_mut()
            .find(|item| item.id == profile.id)
        {
            *existing = profile;
        } else {
            self.config.brokers.push(profile);
        }
        self.refresh_broker_details();
    }

    pub(super) fn open_broker_editor(&mut self, profile: Option<BrokerConfig>) {
        self.editor = Some(BrokerEditor {
            editing_id: profile.as_ref().map(|item| item.id.clone()),
            name: profile
                .as_ref()
                .map_or_else(String::new, |item| item.name.clone()),
            host: profile
                .as_ref()
                .map_or_else(|| "localhost".to_owned(), |item| item.host.clone()),
            port: profile
                .as_ref()
                .map_or_else(|| "1883".to_owned(), |item| item.port.to_string()),
            protocol: profile.as_ref().map_or_else(
                || mqtt::PROTOCOL_MQTT311.to_owned(),
                |item| item.protocol.clone(),
            ),
            subscription: profile
                .as_ref()
                .and_then(|item| {
                    item.subscriptions
                        .first()
                        .map(|subscription| subscription.topic.clone())
                })
                .unwrap_or_else(|| "#".to_owned()),
            username: profile
                .as_ref()
                .and_then(|item| item.auth.username.clone())
                .unwrap_or_default(),
            password: String::new(),
            field: 0,
            error: None,
        });
    }
}

fn handle_editor_key(state: &mut UiState, key: KeyCode) {
    let Some(mut editor) = state.editor.take() else {
        return;
    };
    let editing = editor.editing_id.is_some();
    match key {
        KeyCode::Esc => return,
        KeyCode::Tab | KeyCode::Down | KeyCode::Enter if editor.field < EDITOR_LAST_FIELD => {
            editor.field += 1;
            if editing && editor.field == FIELD_SUBSCRIPTION {
                editor.field += 1;
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            editor.field = editor.field.saturating_sub(1);
            if editing && editor.field == FIELD_SUBSCRIPTION {
                editor.field -= 1;
            }
        }
        KeyCode::Left | KeyCode::Right if editor.field == FIELD_PROTOCOL => {
            editor.protocol = if editor.protocol == mqtt::PROTOCOL_MQTT5 {
                mqtt::PROTOCOL_MQTT311.to_owned()
            } else {
                mqtt::PROTOCOL_MQTT5.to_owned()
            };
        }
        KeyCode::Enter => {
            let port = editor.port.parse::<u16>();
            let error = if editor.name.trim().is_empty() {
                Some("Profile name is required".to_owned())
            } else if editor.host.trim().is_empty() {
                Some("Hostname is required".to_owned())
            } else if state.config.brokers.iter().any(|profile| {
                profile.name == editor.name.trim()
                    && Some(profile.id.as_str()) != editor.editing_id.as_deref()
            }) {
                Some("A profile with this name already exists".to_owned())
            } else if !editing && !valid_topic_filter(&editor.subscription) {
                Some("Subscription must be a valid MQTT topic filter".to_owned())
            } else if !editor.password.is_empty() && editor.username.trim().is_empty() {
                Some("A password requires a username".to_owned())
            } else if port.is_err() || port == Ok(0) {
                Some("Port must be between 1 and 65535".to_owned())
            } else {
                None
            };
            if let Some(error) = error {
                editor.error = Some(error);
                state.editor = Some(editor);
                return;
            }
            let username = Some(editor.username.trim().to_owned()).filter(|name| !name.is_empty());
            let renamed_from = editor.editing_id.clone().filter(|id| {
                state
                    .find_profile(id)
                    .is_some_and(|profile| profile.name != editor.name.trim())
            });
            if let Some(id) = &renamed_from
                && let Some(reason) = state.rename_blocker(id, editor.name.trim())
            {
                editor.error = Some(reason);
                state.editor = Some(editor);
                return;
            }
            let saved = if let Some(id) = editor.editing_id.as_deref() {
                state.broker_store.update(id, |profile| {
                    profile.name = editor.name.trim().to_owned();
                    profile.host = editor.host.trim().to_owned();
                    profile.port = port.expect("port was validated");
                    profile.protocol = editor.protocol.clone();
                    let reference = profile.auth.password_ref.take();
                    profile.auth.password_ref = username
                        .as_ref()
                        .map(|_| reference.unwrap_or_else(|| format!("keyring:{id}")));
                    profile.auth.username = username.clone();
                })
            } else {
                let id = unique_broker_id(
                    editor.name.trim(),
                    state.config.brokers.iter().map(|item| item.id.as_str()),
                );
                state.broker_store.create(BrokerConfig {
                    auth: AuthConfig {
                        password_ref: username.as_ref().map(|_| format!("keyring:{id}")),
                        username: username.clone(),
                    },
                    id,
                    name: editor.name.trim().to_owned(),
                    host: editor.host.trim().to_owned(),
                    port: port.expect("port was validated"),
                    protocol: editor.protocol.clone(),
                    subscriptions: vec![SubscriptionConfig::new(editor.subscription.trim(), 0)],
                    ..BrokerConfig::default()
                })
            };
            let saved = match (saved, &renamed_from) {
                (Ok(profile), Some(_)) => state.broker_store.follow_name(&profile.id),
                (other, _) => other,
            };
            match saved {
                Ok(profile) => {
                    let moved = renamed_from.as_ref().filter(|old| **old != profile.id);
                    if let Some(old) = moved {
                        state.apply_broker_rename(old, &profile.id);
                    }
                    if editor.password.is_empty() {
                        state.notify(
                            match moved {
                                Some(_) => format!(
                                    "Broker profile saved; its folder is now '{}'",
                                    profile.id
                                ),
                                None => "Broker profile saved".to_owned(),
                            },
                            false,
                        );
                    } else {
                        match store_broker_password(&profile, &editor.password) {
                            Ok(()) => state.notify("Broker profile and password saved", false),
                            Err(error) => state.notify(
                                format!(
                                    "Profile saved, but the password was not stored: {error:#}"
                                ),
                                true,
                            ),
                        }
                    }
                    let id = profile.id.clone();
                    state.broker_saved(profile);
                    if moved.is_some() {
                        reload_profiles(state);
                        if let Some(index) =
                            state.config.brokers.iter().position(|item| item.id == id)
                        {
                            state.broker_selection = index;
                        }
                    }
                }
                Err(error) => {
                    editor.error = Some(error.to_string());
                    state.editor = Some(editor);
                }
            }
            return;
        }
        KeyCode::Backspace => match editor.field {
            FIELD_NAME => {
                editor.name.pop();
            }
            FIELD_HOST => {
                editor.host.pop();
            }
            FIELD_PORT => {
                editor.port.pop();
            }
            FIELD_SUBSCRIPTION => {
                editor.subscription.pop();
            }
            FIELD_USERNAME => {
                editor.username.pop();
            }
            FIELD_PASSWORD => {
                editor.password.pop();
            }
            _ => {}
        },
        KeyCode::Char(character) => match editor.field {
            FIELD_NAME => editor.name.push(character),
            FIELD_HOST => editor.host.push(character),
            FIELD_PORT if character.is_ascii_digit() => editor.port.push(character),
            FIELD_SUBSCRIPTION => editor.subscription.push(character),
            FIELD_USERNAME => editor.username.push(character),
            FIELD_PASSWORD => editor.password.push(character),
            _ => {}
        },
        _ => {}
    }
    editor.error = None;
    state.editor = Some(editor);
}

fn handle_broker_manager_key(state: &mut UiState, key: KeyCode) -> UiAction {
    match key {
        KeyCode::Esc | KeyCode::Char('b') => state.view = View::Main,
        KeyCode::Char('q') => return UiAction::Quit,
        KeyCode::Char('x') => return UiAction::Disconnect,
        KeyCode::Char('R') => state.open_replay(),
        KeyCode::Char('a') => state.open_broker_editor(None),
        KeyCode::Char('e') => {
            if let Some(profile) = selected_profile(state).cloned() {
                state.open_broker_editor(Some(profile));
            }
        }
        KeyCode::Char('d') => {
            if let Some(profile) = selected_profile(state) {
                state.confirmation = Some(Confirmation::RemoveBroker(profile.id.clone()));
            }
        }
        KeyCode::Char('v') => {
            if let Some(profile) = selected_profile(state) {
                state.selected_broker = Some(profile.id.clone());
                refresh_certificates(state);
                state.view = View::Certificates;
            }
        }
        KeyCode::Char('u') => duplicate_selected_profile(state),
        KeyCode::Char('s') => {
            if let Some(profile) = selected_profile(state) {
                let id = profile.id.clone();
                state.open_subscriptions(id);
            }
        }
        KeyCode::Char('w') => toggle_publish_permission(state),
        KeyCode::Char('y') => return toggle_system_topics(state),
        KeyCode::Char('*') => toggle_default_profile(state),
        KeyCode::Char('t') => {
            if let Some(mut profile) = selected_profile(state).cloned() {
                // A second session with the same fixed client ID would make the broker drop the active one.
                if state.has_connection(&profile.id) && !profile.unique_client_id {
                    profile.client_id.push_str("-test");
                }
                state
                    .test_results
                    .insert(profile.id.clone(), TestOutcome::Running);
                return UiAction::TestBroker(profile);
            }
        }
        KeyCode::Char(' ') => {
            if let Some(id) = selected_profile(state).map(|profile| profile.id.clone())
                && !state.marked_brokers.remove(&id)
            {
                state.marked_brokers.insert(id);
            }
        }
        KeyCode::Enter => {
            let marked: Vec<BrokerConfig> = state
                .config
                .brokers
                .iter()
                .filter(|profile| state.marked_brokers.contains(&profile.id))
                .cloned()
                .collect();
            if marked.len() > 1 {
                state.view = View::Main;
                return UiAction::ConnectMany(marked);
            }
            // One mark connects that broker; without marks the highlighted one.
            let wanted = marked
                .into_iter()
                .next()
                .or_else(|| selected_profile(state).cloned());
            if let Some(profile) = wanted {
                state.view = View::Main;
                return UiAction::Connect(profile);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.broker_selection = state
                .broker_selection
                .saturating_add(1)
                .min(state.config.brokers.len().saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.broker_selection = state.broker_selection.saturating_sub(1)
        }
        _ => {}
    }
    UiAction::None
}

fn selected_profile(state: &UiState) -> Option<&BrokerConfig> {
    state.config.brokers.get(state.broker_selection)
}

fn duplicate_selected_profile(state: &mut UiState) {
    let Some(id) = selected_profile(state).map(|profile| profile.id.clone()) else {
        return;
    };
    match state.broker_store.duplicate_profile(&id) {
        Ok(copy) => {
            reload_profiles(state);
            if let Some(index) = state
                .config
                .brokers
                .iter()
                .position(|profile| profile.id == copy.id)
            {
                state.broker_selection = index;
            }
            state.notify(
                format!(
                    "Duplicated as {}; imported certificates are not copied",
                    copy.name
                ),
                false,
            );
        }
        Err(error) => state.notify(format!("{error:#}"), true),
    }
}

fn toggle_publish_permission(state: &mut UiState) {
    let Some((id, name, allowed)) = selected_profile(state).map(|profile| {
        (
            profile.id.clone(),
            profile.name.clone(),
            profile.allow_publish,
        )
    }) else {
        return;
    };
    match state
        .broker_store
        .update(&id, |profile| profile.allow_publish = !allowed)
    {
        Ok(_) => {
            reload_profiles(state);
            let active = state.has_connection(&id);
            state.notify(
                format!(
                    "Publishing {} for {name}{}",
                    if allowed { "disabled" } else { "enabled" },
                    if active {
                        "; applies after reconnecting"
                    } else {
                        ""
                    }
                ),
                false,
            );
        }
        Err(error) => state.notify(format!("{error:#}"), true),
    }
}

/// Shows or hides the `$SYS/#` topics of the selected broker; applied live when it is connected.
fn toggle_system_topics(state: &mut UiState) -> UiAction {
    let Some((id, name, shown, explicit)) = selected_profile(state).map(|profile| {
        (
            profile.id.clone(),
            profile.name.clone(),
            profile.show_system_topics,
            profile.subscriptions.iter().any(|entry| {
                entry.enabled && !entry.exclude && entry.topic == mate::config::SYSTEM_TOPICS
            }),
        )
    }) else {
        return UiAction::None;
    };
    if let Err(error) = state
        .broker_store
        .update(&id, |profile| profile.show_system_topics = !shown)
    {
        state.notify(format!("{error:#}"), true);
        return UiAction::None;
    }
    reload_profiles(state);
    let topic = mate::config::SYSTEM_TOPICS.to_owned();
    let live = state.has_connection(&id);
    state.notify(
        if shown {
            format!("System topics hidden for {name}; topics already received stay in the list")
        } else {
            format!("System topics ($SYS/#) shown for {name}")
        },
        false,
    );
    match (live, shown, explicit) {
        (true, false, _) => {
            UiAction::Subscription(id, SubscriptionChange::Subscribe { topic, qos: 0 })
        }
        (true, true, false) => {
            UiAction::Subscription(id, SubscriptionChange::Unsubscribe { topic })
        }
        _ => UiAction::None,
    }
}

fn toggle_default_profile(state: &mut UiState) {
    let Some(id) = selected_profile(state).map(|profile| profile.id.clone()) else {
        return;
    };
    let make_default = state.config.general.default_broker.as_deref() != Some(id.as_str());
    match state
        .broker_store
        .set_default_broker(make_default.then_some(id.as_str()))
    {
        Ok(()) => {
            state.config.general.default_broker = make_default.then(|| id.clone());
            state.notify(
                if make_default {
                    "Default broker set"
                } else {
                    "Default broker cleared"
                },
                false,
            );
        }
        Err(error) => state.notify(format!("{error:#}"), true),
    }
}

fn handle_certificate_manager_key(state: &mut UiState, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Char('b') => state.view = View::Brokers,
        KeyCode::Up | KeyCode::Char('k') => {
            state.certificate_selection = state.certificate_selection.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.certificate_selection = state
                .certificate_selection
                .saturating_add(1)
                .min(state.certificates.len().saturating_sub(1));
        }
        KeyCode::Char('i') => open_certificate_editor(state, CertificateEditMode::Import),
        KeyCode::Char('r') => open_certificate_editor(state, CertificateEditMode::Reference),
        KeyCode::Char('d') => {
            if let Some(certificate) = state.certificates.get(state.certificate_selection) {
                state.confirmation = Some(Confirmation::RemoveCertificate(certificate.kind));
            }
        }
        _ => {}
    }
}

fn open_certificate_editor(state: &mut UiState, mode: CertificateEditMode) {
    state.certificate_editor = Some(CertificateEditor {
        mode,
        kind: CertificateKind::Ca,
        path: String::new(),
        cursor: 0,
        field: 0,
        replace: false,
        error: None,
    });
}

fn handle_certificate_editor_key(state: &mut UiState, key: KeyCode) {
    let Some(mut editor) = state.certificate_editor.take() else {
        return;
    };
    match key {
        KeyCode::Esc => return,
        KeyCode::Tab | KeyCode::Down | KeyCode::Up => editor.field = (editor.field + 1) % 2,
        KeyCode::Char('!') if matches!(editor.mode, CertificateEditMode::Import) => {
            editor.replace = !editor.replace
        }
        KeyCode::Char('1') if editor.field == 0 => editor.kind = CertificateKind::Ca,
        KeyCode::Char('2') if editor.field == 0 => editor.kind = CertificateKind::ClientCertificate,
        KeyCode::Char('3') if editor.field == 0 => editor.kind = CertificateKind::ClientPrivateKey,
        KeyCode::Backspace if editor.field == 1 => {
            publish::backspace_at(&mut editor.path, &mut editor.cursor);
        }
        KeyCode::Delete if editor.field == 1 => publish::delete_at(&mut editor.path, editor.cursor),
        KeyCode::Left if editor.field == 1 => editor.cursor = editor.cursor.saturating_sub(1),
        KeyCode::Right if editor.field == 1 => {
            editor.cursor = (editor.cursor + 1).min(editor.path.chars().count());
        }
        KeyCode::Home if editor.field == 1 => editor.cursor = 0,
        KeyCode::End if editor.field == 1 => editor.cursor = editor.path.chars().count(),
        KeyCode::Char(character) if editor.field == 1 => {
            publish::insert_at(
                &mut editor.path,
                &mut editor.cursor,
                character.encode_utf8(&mut [0; 4]),
            );
        }
        KeyCode::Enter if !editor.path.trim().is_empty() => {
            let broker_id = match state.selected_broker.as_deref() {
                Some(id) => id,
                None => {
                    editor.error = Some("No broker selected".to_owned());
                    state.certificate_editor = Some(editor);
                    return;
                }
            };
            let service = CertificateService::new(state.broker_store.clone());
            let path = PathBuf::from(editor.path.trim());
            let result = match editor.mode {
                CertificateEditMode::Import => service
                    .import(broker_id, editor.kind, &path, editor.replace)
                    .map(|_| ()),
                CertificateEditMode::Reference => {
                    service.set_reference(broker_id, editor.kind, &path)
                }
            };
            match result {
                Ok(()) => {
                    state.notify("Certificate configuration saved", false);
                    reload_profiles(state);
                    refresh_certificates(state);
                }
                Err(error) => {
                    editor.error = Some(error.to_string());
                    state.certificate_editor = Some(editor);
                }
            }
            return;
        }
        _ => {}
    }
    editor.error = None;
    state.certificate_editor = Some(editor);
}

fn handle_confirmation_key(state: &mut UiState, key: KeyCode) {
    let Some(confirmation) = state.confirmation.take() else {
        return;
    };
    match key {
        KeyCode::Char('y') => match confirmation {
            Confirmation::RemoveBroker(id) => {
                let result = (|| {
                    if state.config.general.default_broker.as_deref() == Some(&id) {
                        state.config.general.default_broker = None;
                        state.broker_store.save_general(&state.config)?;
                    }
                    state.broker_store.remove(&id)
                })();
                match result {
                    Ok(()) => {
                        state.config.brokers.retain(|profile| profile.id != id);
                        state.broker_selection = state
                            .broker_selection
                            .min(state.config.brokers.len().saturating_sub(1));
                        state.refresh_broker_details();
                        state.notify("Broker profile removed", false);
                    }
                    Err(error) => state.notify(error.to_string(), true),
                }
            }
            Confirmation::Quit => {}
            Confirmation::RemoveCertificate(kind) => {
                let result = state
                    .selected_broker
                    .as_deref()
                    .context("No broker selected")
                    .and_then(|id| {
                        CertificateService::new(state.broker_store.clone()).remove(id, kind)
                    });
                match result {
                    Ok(()) => {
                        state.notify("Certificate configuration removed", false);
                        reload_profiles(state);
                        refresh_certificates(state);
                    }
                    Err(error) => state.notify(error.to_string(), true),
                }
            }
        },
        KeyCode::Char('n') | KeyCode::Esc => {}
        _ => state.confirmation = Some(confirmation),
    }
}

/// Re-reads profiles after certificate changes so the TLS column reflects the files on disk.
fn reload_profiles(state: &mut UiState) {
    if let Ok(config) = state.broker_store.load() {
        state.config.brokers = config.brokers;
    }
    state.refresh_broker_details();
}

fn refresh_certificates(state: &mut UiState) {
    state.certificates = state
        .selected_broker
        .as_deref()
        .and_then(|id| {
            CertificateService::new(state.broker_store.clone())
                .list(id)
                .ok()
        })
        .unwrap_or_default();
    state.certificate_selection = state
        .certificate_selection
        .min(state.certificates.len().saturating_sub(1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use layout::Panel;
    use mate::mqtt::IncomingMessage;
    use ratatui::{Terminal, backend::TestBackend};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use std::time::SystemTime;

    fn temp_store() -> (BrokerStore, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        (store, directory)
    }

    fn profile(store: &BrokerStore, name: &str) -> BrokerConfig {
        store
            .create(BrokerConfig {
                name: name.to_owned(),
                host: format!("{}.example.test", name.to_lowercase()),
                ..BrokerConfig::default()
            })
            .unwrap()
    }

    fn message(topic: &str, payload: &str) -> BrokerEvent {
        message_with(topic, payload.as_bytes(), false)
    }

    fn message_with(topic: &str, payload: &[u8], retain: bool) -> BrokerEvent {
        BrokerEvent::Message(IncomingMessage {
            topic: topic.to_owned(),
            payload: payload.to_vec(),
            qos: 0,
            retain,
            dup: false,
            properties: None,
            received: Instant::now(),
            wall: SystemTime::now(),
        })
    }

    fn connected(state: &mut UiState, profile: &BrokerConfig) {
        state.begin_connection(profile);
        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Connected,
            detail: None,
        });
    }

    /// A state connected to "Production" with the given topics already received.
    fn state_with(topics: &[(&str, &str)]) -> (UiState, tempfile::TempDir) {
        let (store, directory) = temp_store();
        let production = profile(&store, "Production");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        for (topic, payload) in topics {
            state.apply_broker_event(message(topic, payload));
        }
        (state, directory)
    }

    fn screen(state: &mut UiState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(state: &mut UiState, keys: &str) {
        for character in keys.chars() {
            handle_key(state, KeyCode::Char(character));
        }
    }

    #[test]
    fn profile_wizard_saves_a_reloadable_broker() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);

        press(&mut state, "ba");
        press(&mut state, "test-broker");
        for _ in 0..7 {
            handle_key(&mut state, KeyCode::Enter);
        }

        assert!(state.editor.is_none());
        assert_eq!(state.selected_broker.as_deref(), Some("test-broker"));
        assert_eq!(state.config.brokers.len(), 1);
        let saved = store.get("test-broker").unwrap();
        assert_eq!(saved.host, "localhost");
        assert_eq!(saved.subscriptions[0].topic, "#");
        assert!(saved.auth.username.is_none() && saved.auth.password_ref.is_none());
    }

    #[test]
    fn profile_wizard_sets_keyring_reference_for_username_and_rejects_lone_password() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);

        press(&mut state, "ba");
        press(&mut state, "secured");
        for _ in 0..6 {
            handle_key(&mut state, KeyCode::Enter);
        }
        press(&mut state, "secret");
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.editor.as_ref().unwrap().error.is_some());
        assert!(store.get("secured").is_err());

        // Fill the username; the password stays empty so the keyring is not touched.
        state.editor.as_mut().unwrap().password.clear();
        state.editor.as_mut().unwrap().field = FIELD_USERNAME;
        press(&mut state, "admin");
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Enter);

        let saved = store.get("secured").unwrap();
        assert_eq!(saved.auth.username.as_deref(), Some("admin"));
        assert_eq!(saved.auth.password_ref.as_deref(), Some("keyring:secured"));
    }

    #[test]
    fn broker_editor_chooses_mqtt5_and_edit_keeps_the_subscription_list() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);

        press(&mut state, "ba");
        press(&mut state, "modern");
        for _ in 0..3 {
            handle_key(&mut state, KeyCode::Tab);
        }
        handle_key(&mut state, KeyCode::Right);
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Enter);
        }
        assert!(state.editor.is_none());
        let saved = store.get("modern").unwrap();
        assert_eq!(saved.protocol, "mqtt5");

        store.add_subscription("modern", "plant/#", 1).unwrap();
        state.view = View::Brokers;
        handle_key(&mut state, KeyCode::Char('e'));
        for _ in 0..6 {
            handle_key(&mut state, KeyCode::Enter);
        }
        let edited = store.get("modern").unwrap();
        assert_eq!(edited.protocol, "mqtt5");
        assert_eq!(edited.subscriptions.len(), 2);
    }

    #[test]
    fn info_view_shows_message_facts_statistics_and_closes_with_escape() {
        let (mut state, _directory) = state_with(&[
            ("plant/line1", r#"{"a":1}"#),
            ("plant/line1", r#"{"a":2}"#),
            ("plant/line2", "text"),
        ]);
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "ji");
        let text = screen(&mut state, 120, 40);
        assert!(text.contains("TOPIC METADATA"), "{text}");
        for expected in [
            "plant/line1",
            "JSON object",
            "differs from the previous message",
            "2 since connecting",
            "not available (MQTT 3.1.1",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }

        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.main.inspector.info);
        assert!(screen(&mut state, 120, 40).contains("PAYLOAD INSPECTOR"));
    }

    #[test]
    fn user_properties_are_visible_in_the_inspector_without_opening_the_info_view() {
        let (mut state, _directory) = state_with(&[]);
        state.apply_broker_event(BrokerEvent::Message(IncomingMessage {
            topic: "plant/line1".to_owned(),
            payload: b"{}".to_vec(),
            qos: 0,
            retain: false,
            dup: false,
            properties: Some(std::sync::Arc::new(mate::topics::MessageProperties {
                user_properties: vec![
                    ("origin".to_owned(), "plc-7".to_owned()),
                    ("site".to_owned(), "hall 2".to_owned()),
                ],
                content_type: Some("application/json".to_owned()),
                ..Default::default()
            })),
            received: Instant::now(),
            wall: SystemTime::now(),
        }));
        state.apply_broker_event(message("plant/line2", "plain"));
        handle_key(&mut state, KeyCode::Right);
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 140, 24);
        assert!(
            text.contains("MQTT 5")
                && text.contains("origin=plc-7")
                && text.contains("site=hall 2"),
            "{text}"
        );
        assert!(text.contains("type application/json"), "{text}");

        // The info view lists them on its first screen, even on a small terminal.
        press(&mut state, "i");
        let text = screen(&mut state, 100, 24);
        assert!(
            text.contains("User property")
                && text.contains("origin = plc-7")
                && text.contains("site = hall 2"),
            "{text}"
        );
        press(&mut state, "i");

        // A message without properties shows no such line.
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Down);
        let text = screen(&mut state, 140, 24);
        assert!(!text.contains("MQTT 5  "), "{text}");
    }

    #[test]
    fn arrow_keys_move_between_the_fields_of_the_broker_editor() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store, None);
        press(&mut state, "ba");
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.editor.as_ref().unwrap().field, FIELD_PORT);
        handle_key(&mut state, KeyCode::Up);
        assert_eq!(state.editor.as_ref().unwrap().field, FIELD_HOST);
        handle_key(&mut state, KeyCode::Up);
        handle_key(&mut state, KeyCode::Up);
        assert_eq!(
            state.editor.as_ref().unwrap().field,
            FIELD_NAME,
            "stops at the first field"
        );
        for _ in 0..10 {
            handle_key(&mut state, KeyCode::Down);
        }
        assert_eq!(
            state.editor.as_ref().unwrap().field,
            FIELD_PASSWORD,
            "stops at the last field"
        );

        // Editing an existing profile skips the subscription field in both directions.
        state.editor = None;
        let broker = state
            .broker_store
            .create(BrokerConfig {
                name: "x".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        state.open_broker_editor(Some(broker));
        state.editor.as_mut().unwrap().field = FIELD_PROTOCOL;
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.editor.as_ref().unwrap().field, FIELD_USERNAME);
        handle_key(&mut state, KeyCode::Up);
        assert_eq!(state.editor.as_ref().unwrap().field, FIELD_PROTOCOL);
    }

    #[test]
    fn info_view_lists_mqtt5_properties_and_subtree_totals() {
        let (mut state, _directory) = state_with(&[("plant/line2", "x")]);
        state.apply_broker_event(BrokerEvent::Message(IncomingMessage {
            topic: "plant/line1".to_owned(),
            payload: b"{}".to_vec(),
            qos: 1,
            retain: true,
            dup: false,
            properties: Some(std::sync::Arc::new(mate::topics::MessageProperties {
                content_type: Some("application/json".to_owned()),
                user_properties: vec![("origin".to_owned(), "plc-7".to_owned())],
                message_expiry_secs: Some(60),
                ..Default::default()
            })),
            received: Instant::now(),
            wall: SystemTime::now(),
        }));
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "ji");
        let text = screen(&mut state, 120, 40);
        for expected in [
            "application/json",
            "origin = plc-7",
            "60 s",
            "Retained           yes",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }

        // The group row shows totals for everything below it.
        press(&mut state, "k");
        handle_key(&mut state, KeyCode::Up);
        let text = screen(&mut state, 120, 40);
        assert!(
            text.contains("SUBTREE") && text.contains("Topics below"),
            "{text}"
        );
    }

    #[test]
    fn topic_rows_and_the_history_title_count_messages_since_the_last_connection_attempt() {
        let (mut state, _directory) = state_with(&[("plant/line1", "1"), ("plant/line1", "2")]);
        handle_key(&mut state, KeyCode::Right);
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("\u{d7}2"), "{text}");

        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Down);
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("HISTORY \u{b7} 2"), "{text}");

        for _ in 0..1_498 {
            state.apply_broker_event(message("plant/line1", "x"));
        }
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("\u{d7}1.5k") && text.contains("HISTORY \u{b7} 1.5k"),
            "{text}"
        );

        // A new connection attempt starts the count again; the history itself is kept.
        let broker = state.config.brokers[0].clone();
        connected(&mut state, &broker);
        let text = screen(&mut state, 120, 20);
        assert!(
            !text.contains("\u{d7}") && !text.contains("HISTORY \u{b7}"),
            "{text}"
        );
        state.apply_broker_event(message("plant/line1", "y"));
        assert!(screen(&mut state, 120, 20).contains("\u{d7}1"));
    }

    #[test]
    fn ctrl_s_in_the_history_sends_the_selected_message_again_exactly_as_received() {
        let ctrl_s = || KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let (mut state, _directory) = writable_state(&[]);
        state.apply_broker_event(BrokerEvent::Message(IncomingMessage {
            topic: "plant/line1".to_owned(),
            payload: vec![0x00, 0xff, b'{', b'}'],
            qos: 1,
            retain: true,
            dup: false,
            properties: None,
            received: Instant::now(),
            wall: SystemTime::now(),
        }));
        state.apply_broker_event(message("plant/line1", "newer"));
        handle_key(&mut state, KeyCode::Right);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.main.selected.as_deref(), Some("plant/line1"));

        // Not in the history: nothing happens, so a stray Ctrl+S cannot publish.
        assert!(matches!(
            handle_key_event(&mut state, ctrl_s()),
            UiAction::None
        ));

        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::History);
        screen(&mut state, 120, 20);
        match handle_key_event(&mut state, ctrl_s()) {
            UiAction::Publish(_, request) => {
                assert_eq!(request.payload, b"newer");
                assert_eq!(
                    (request.topic.as_str(), request.qos, request.retain),
                    ("plant/line1", 0, false)
                );
            }
            _ => panic!("expected the newest message to be sent again"),
        }

        // The older message keeps its raw bytes, QoS and retain flag.
        handle_key(&mut state, KeyCode::Down);
        match handle_key_event(&mut state, ctrl_s()) {
            UiAction::Publish(_, request) => {
                assert_eq!(request.payload, vec![0x00, 0xff, b'{', b'}']);
                assert_eq!((request.qos, request.retain), (1, true));
            }
            _ => panic!("expected the older message to be sent again"),
        }

        // A pinned message can be sent from the inspector as well.
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(state.main.focus, Panel::Inspector);
        assert!(matches!(
            handle_key_event(&mut state, ctrl_s()),
            UiAction::Publish(..)
        ));
    }

    #[test]
    fn republishing_from_the_history_respects_the_publish_permission() {
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let (mut state, _directory) = state_with(&[("cmd", "1")]);
        handle_key(&mut state, KeyCode::Right);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert!(matches!(
            handle_key_event(&mut state, ctrl_s),
            UiAction::None
        ));
        let notice = state.notice.as_ref().unwrap();
        assert!(notice.error && notice.text.contains("Publishing is off"));
    }

    #[test]
    fn plus_and_minus_expand_and_collapse_the_whole_topic_tree() {
        let (mut state, _directory) = state_with(&[
            ("plant/line1/temp", "1"),
            ("plant/line2/temp", "2"),
            ("site/id", "3"),
        ]);
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("plant") && !text.contains("line1"), "{text}");

        press(&mut state, "+");
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("line1") && text.contains("line2") && text.contains("id"),
            "{text}"
        );
        assert_eq!(state.main.rows.len(), 7);

        // The selected leaf survives as the closest visible ancestor after collapsing.
        for _ in 0..2 {
            handle_key(&mut state, KeyCode::Down);
        }
        assert_eq!(state.main.selected.as_deref(), Some("plant/line1/temp"));
        press(&mut state, "-");
        screen(&mut state, 120, 20);
        assert_eq!(state.main.rows.len(), 2);
        assert_eq!(state.main.selected.as_deref(), Some("plant"));
        assert!(screen(&mut state, 200, 20).contains("[+/-] Expand/collapse all"));
    }

    #[test]
    fn shift_arrows_open_and_close_the_selected_branch_with_everything_below() {
        let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        let (mut state, _directory) = state_with(&[
            ("plant/line1/temp", "1"),
            ("plant/line2/temp", "2"),
            ("site/id", "3"),
        ]);
        // On "plant": Shift+Right opens plant, line1 and line2 but leaves "site" alone.
        handle_key_event(&mut state, shift(KeyCode::Right));
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("line1") && text.contains("line2") && !text.contains("id"),
            "{text}"
        );
        assert_eq!(
            state.main.rows.len(),
            6,
            "plant, 2 lines with temp each, site"
        );

        // A plain Left collapses only one level; Shift+Left closes the whole subtree.
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.main.selected.as_deref(), Some("plant/line1/temp"));
        handle_key_event(&mut state, shift(KeyCode::Left));
        assert_eq!(state.main.selected.as_deref(), Some("plant/line1"));
        assert_eq!(state.main.rows.len(), 5, "line1 closed, line2 still open");

        handle_key(&mut state, KeyCode::Up);
        handle_key_event(&mut state, shift(KeyCode::Left));
        assert_eq!(state.main.rows.len(), 2);
        handle_key_event(&mut state, shift(KeyCode::Right));
        assert_eq!(
            state.main.rows.len(),
            6,
            "reopening restores every level below plant"
        );

        // Other panels keep their own arrow behavior.
        handle_key(&mut state, KeyCode::Tab);
        let before = state.main.rows.len();
        handle_key_event(&mut state, shift(KeyCode::Left));
        assert_eq!(state.main.rows.len(), before);
    }

    #[test]
    fn subscription_manager_adds_toggles_and_removes_and_persists() {
        let (store, _directory) = temp_store();
        let broker = profile(&store, "Prod");
        store.add_subscription(&broker.id, "#", 0).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        state.view = View::Brokers;

        handle_key(&mut state, KeyCode::Char('s'));
        assert_eq!(state.view, View::Subscriptions);
        let shown = screen(&mut state, 100, 24);
        assert!(shown.contains("SUBSCRIPTIONS") && shown.contains("[x] #"));

        handle_key(&mut state, KeyCode::Char('a'));
        press(&mut state, "plant/+/temp");
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "1");
        let action = handle_key(&mut state, KeyCode::Enter);
        assert!(matches!(action, UiAction::None), "broker is not connected");
        let saved = store.get(&broker.id).unwrap();
        assert_eq!(saved.subscriptions.len(), 2);
        assert_eq!(saved.subscriptions[1].topic, "plant/+/temp");
        assert_eq!(saved.subscriptions[1].qos, 1);
        assert!(saved.subscriptions[1].enabled);

        handle_key(&mut state, KeyCode::Char(' '));
        assert!(!store.get(&broker.id).unwrap().subscriptions[1].enabled);
        assert!(screen(&mut state, 100, 24).contains("[ ] plant/+/temp"));

        press(&mut state, "2");
        assert_eq!(store.get(&broker.id).unwrap().subscriptions[1].qos, 2);

        handle_key(&mut state, KeyCode::Char('d'));
        assert_eq!(store.get(&broker.id).unwrap().subscriptions.len(), 1);
        assert_eq!(state.config.brokers[0].subscriptions.len(), 1);
    }

    #[test]
    fn subscription_changes_reach_the_running_connection_only() {
        let (store, _directory) = temp_store();
        let broker = profile(&store, "Prod");
        let other = profile(&store, "Other");
        store.add_subscription(&broker.id, "#", 0).unwrap();
        store.add_subscription(&other.id, "#", 0).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        connected(&mut state, &broker);

        state.open_subscriptions(broker.id.clone());
        let action = handle_key(&mut state, KeyCode::Char(' '));
        assert!(matches!(
            action,
            UiAction::Subscription(_, SubscriptionChange::Unsubscribe { ref topic }) if topic == "#"
        ));
        let action = handle_key(&mut state, KeyCode::Char(' '));
        assert!(matches!(
            action,
            UiAction::Subscription(_, SubscriptionChange::Subscribe { ref topic, qos: 0 }) if topic == "#"
        ));

        state.open_subscriptions(other.id.clone());
        let action = handle_key(&mut state, KeyCode::Char(' '));
        assert!(matches!(action, UiAction::None));
    }

    #[test]
    fn subscription_editor_rejects_invalid_filters() {
        let (store, _directory) = temp_store();
        let broker = profile(&store, "Prod");
        store.add_subscription(&broker.id, "#", 0).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        state.open_subscriptions(broker.id.clone());

        handle_key(&mut state, KeyCode::Char('a'));
        press(&mut state, "a/#/b");
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Enter);

        let editor = state.subscriptions.as_ref().unwrap().editor.as_ref();
        assert!(editor.unwrap().error.is_some());
        assert_eq!(store.get(&broker.id).unwrap().subscriptions.len(), 1);
        assert!(screen(&mut state, 100, 24).contains("not a valid MQTT topic filter"));
    }

    #[test]
    fn broker_manager_edit_preserves_id_and_updates_profile() {
        let (store, _directory) = temp_store();
        let profile = profile(&store, "test");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);

        handle_key(&mut state, KeyCode::Char('b'));
        handle_key(&mut state, KeyCode::Char('e'));
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "renamed");
        for _ in 0..6 {
            handle_key(&mut state, KeyCode::Enter);
        }

        // The folder (and ID) follows the new name; the old one is gone.
        assert!(store.get(&profile.id).is_err());
        let updated = store.get("renamed").unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(state.config.brokers[0].id, "renamed");
        assert_eq!(state.selected_broker.as_deref(), Some("renamed"));
    }

    #[test]
    fn a_duplicate_that_is_renamed_gets_a_folder_with_the_new_name_and_keeps_its_state() {
        let (store, directory) = temp_store();
        let original = profile(&store, "Production");
        store.set_default_broker(Some(&original.id)).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);

        press(&mut state, "bu");
        let copy = state
            .config
            .brokers
            .iter()
            .find(|profile| profile.name == "Production copy")
            .expect("the copy is listed")
            .clone();
        assert_eq!(copy.id, "production-copy");
        state.broker_selection = state
            .config
            .brokers
            .iter()
            .position(|profile| profile.id == copy.id)
            .unwrap();
        state.publish_history.insert(
            copy.id.clone(),
            vec![mate::ui_state::PublishHistoryEntry {
                topic: "t".into(),
                payload: "p".into(),
                qos: 0,
                retain: false,
                sent_at: 1,
            }],
        );

        press(&mut state, "e");
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "Staging");
        for _ in 0..6 {
            handle_key(&mut state, KeyCode::Enter);
        }

        let brokers = directory.path().join("brokers");
        assert!(brokers.join("production-staging").is_dir());
        assert!(
            !brokers.join("production-copy").exists(),
            "the old folder is gone"
        );
        let renamed = store.get("production-staging").unwrap();
        assert_eq!(renamed.name, "Production Staging");
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("folder is now")
        );
        assert!(
            state
                .config
                .brokers
                .iter()
                .all(|profile| profile.id != "production-copy"),
            "the list was refreshed"
        );
        assert_eq!(
            state.config.brokers[state.broker_selection].id,
            "production-staging",
        );
        assert!(state.publish_history.contains_key("production-staging"));
        assert!(!state.publish_history.contains_key("production-copy"));
        assert_eq!(
            state.config.general.default_broker.as_deref(),
            Some(original.id.as_str()),
            "the default broker was another profile and stays"
        );
    }

    #[test]
    fn a_connected_broker_cannot_be_renamed_until_it_is_disconnected() {
        let (store, _directory) = temp_store();
        let alpha = profile(&store, "Alpha");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        connected(&mut state, &alpha);

        press(&mut state, "be");
        for _ in 0..5 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "Beta");
        for _ in 0..6 {
            handle_key(&mut state, KeyCode::Enter);
        }
        let editor = state.editor.as_ref().expect("the editor stays open");
        assert!(editor.error.as_ref().unwrap().contains("Disconnect first"));
        assert!(store.get("alpha").is_ok());

        // Without a running connection it works, also for a broker that was connected before.
        state.mark_disconnected();
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.editor.is_none());
        assert!(store.get("beta").is_ok() && store.get("alpha").is_err());
        assert!(
            state.connections.is_empty(),
            "the cached session of the old ID was dropped"
        );
    }

    #[test]
    fn ctrl_v_asks_for_the_clipboard_and_never_types_a_v() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_PAYLOAD
        );

        ctrl(&mut state, 'v');
        assert!(state.take_paste_request());
        assert!(!state.take_paste_request(), "the request is taken once");
        ctrl(&mut state, 'x');
        assert_eq!(state.publish_editor.as_ref().unwrap().payload, "");

        // What the event loop does with the clipboard text: line breaks of every kind become \n.
        handle_paste(&mut state, "{\r\n  \"a\": 1,\r  \"b\": [1, 2]\n}");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.payload, "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}");
        assert_eq!(editor.cursor, editor.payload.chars().count());
        assert_eq!(editor.field, publish::FIELD_PAYLOAD);

        // AltGr (Ctrl+Alt on Windows) types characters such as { on a German keyboard.
        handle_key_event(
            &mut state,
            KeyEvent::new(
                KeyCode::Char('['),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        );
        assert!(
            state
                .publish_editor
                .as_ref()
                .unwrap()
                .payload
                .ends_with("}[")
        );

        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
        );
        assert!(state.take_paste_request());
    }

    #[test]
    fn a_paste_is_typed_into_one_line_fields_and_ignored_elsewhere() {
        let (mut state, _directory) = state_with(&[("plc/a", "1"), ("plc/b", "2")]);
        handle_paste(&mut state, "plc");
        assert!(
            !state.main.search.active,
            "no field has the keyboard: nothing happens"
        );

        press(&mut state, "S/");
        handle_paste(&mut state, "  plc/b\n");
        assert_eq!(state.stream_view.topic_filter, "plc/b");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.stream_view.shown().len(), 1);

        handle_key(&mut state, KeyCode::Esc);
        handle_key(&mut state, KeyCode::Esc);
        press(&mut state, "/");
        handle_paste(&mut state, "plc/a");
        assert_eq!(state.main.search.query, "plc/a");
    }

    #[test]
    fn broker_manager_requires_confirmation_before_removal() {
        let (store, _directory) = temp_store();
        let profile = profile(&store, "test");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        state.view = View::Brokers;

        handle_key(&mut state, KeyCode::Char('d'));
        assert!(store.get(&profile.id).is_ok());
        handle_key(&mut state, KeyCode::Char('y'));
        assert!(store.get(&profile.id).is_err());
        assert!(state.config.brokers.is_empty());
    }

    #[test]
    fn certificate_manager_imports_a_valid_ca_file() {
        let (store, directory) = temp_store();
        let profile = profile(&store, "test");
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let certificate = params.self_signed(&key).unwrap();
        let source = directory.path().join("ca.pem");
        std::fs::write(&source, certificate.pem()).unwrap();

        let mut state = UiState::new(
            store.load().unwrap(),
            store.clone(),
            Some(profile.id.clone()),
        );
        state.view = View::Certificates;
        open_certificate_editor(&mut state, CertificateEditMode::Import);
        handle_key(&mut state, KeyCode::Tab);
        for character in source.to_string_lossy().chars() {
            handle_key(&mut state, KeyCode::Char(character));
        }
        handle_key(&mut state, KeyCode::Enter);

        let saved = store.get(&profile.id).unwrap();
        assert_eq!(saved.tls.ca_file, Some(PathBuf::from("certs/ca.pem")));
        assert_eq!(state.certificates.len(), 1);
        assert!(state.certificates[0].imported);
    }

    #[test]
    fn header_reflects_one_coherent_connection_state() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let _second = profile(&store, "Second");
        let mut state = UiState::new(store.load().unwrap(), store, None);

        let idle = screen(&mut state, 120, 20);
        assert!(idle.contains("No broker selected"));
        assert!(!idle.contains("Connected"));
        state.selected_broker = Some(production.id.clone());
        assert!(screen(&mut state, 120, 20).contains("○ Production"));
        state.selected_broker = None;

        state.begin_connection(&production);
        let connecting = screen(&mut state, 120, 20);
        assert!(connecting.contains("Production") && connecting.contains("Connecting"));

        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Connected,
            detail: None,
        });
        let online = screen(&mut state, 120, 20);
        assert!(online.contains("● Production"), "{online}");
        assert!(online.contains("Connected") && online.contains("MQTT 3.1.1"));
        assert!(online.contains("plain") && online.contains("msg/s"));
        assert!(!online.contains("TLS"));

        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Reconnecting,
            detail: Some("Connection refused".into()),
        });
        let retrying = screen(&mut state, 120, 20);
        assert!(retrying.contains("Reconnecting"));
        assert!(retrying.contains("Last error: Connection refused"));
        assert!(
            !retrying.contains("MQTT 3.1.1"),
            "negotiated values only while connected"
        );

        state.apply_broker_event(BrokerEvent::Failed("Authentication failed".into()));
        let failed = screen(&mut state, 120, 20);
        assert!(failed.contains("Error") && failed.contains("Authentication failed"));

        state.mark_disconnected();
        let offline = screen(&mut state, 120, 20);
        assert!(offline.contains("○ Production") && offline.contains("Disconnected"));
    }

    #[test]
    fn tls_is_only_reported_for_an_established_tls_connection() {
        let (store, _directory) = temp_store();
        let mut secure = profile(&store, "Secure");
        secure.tls.enabled = true;
        let mut state = UiState::new(store.load().unwrap(), store, None);
        state.begin_connection(&secure);
        assert!(!screen(&mut state, 120, 20).contains("TLS"));
        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Connected,
            detail: None,
        });
        assert!(screen(&mut state, 120, 20).contains("TLS ✓"));
    }

    #[test]
    fn header_shows_the_connected_broker_not_the_selected_one() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let _development = profile(&store, "Development");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        state.selected_broker = Some("development".to_owned());
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("Production") && text.contains("Connected"));
        assert!(!text.contains("Development"));
    }

    #[test]
    fn switching_brokers_discards_the_previous_brokers_data() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let development = profile(&store, "Development");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        state.apply_broker_event(message("plant/status", "RUNNING"));
        assert_eq!(state.topics.topic_count(), 1);

        state.mark_disconnected();
        state.begin_connection(&production);
        assert_eq!(state.topics.topic_count(), 1, "same broker keeps its cache");

        state.begin_connection(&development);
        assert_eq!(state.topics.topic_count(), 0);
        assert!(state.has_connection(&development.id));
        assert!(screen(&mut state, 120, 20).contains("Development"));
    }

    #[test]
    fn messages_without_a_connection_are_ignored() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store, None);
        state.apply_broker_event(message("a", "1"));
        assert_eq!(state.topics.topic_count(), 0);
    }

    #[test]
    fn data_source_distinguishes_live_retained_and_cached_messages() {
        let (mut state, _directory) = state_with(&[("old", "1")]);
        // Reconnect: everything stored so far predates the new connection.
        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Reconnecting,
            detail: None,
        });
        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Connected,
            detail: None,
        });
        state.apply_broker_event(message_with("fresh", b"2", false));
        state.apply_broker_event(message_with("kept", b"3", true));

        let source = |state: &UiState, topic: &str| {
            state.data_source(state.topics.latest_message(topic).unwrap())
        };
        assert_eq!(source(&state, "old"), DataSource::Cached);
        assert_eq!(source(&state, "fresh"), DataSource::Live);
        assert_eq!(source(&state, "kept"), DataSource::Retained);

        state.mark_disconnected();
        assert_eq!(source(&state, "fresh"), DataSource::Cached);
    }

    #[test]
    fn topic_explorer_expands_navigates_and_preserves_selected_path() {
        let (mut state, _directory) = state_with(&[
            ("plant/line-1/status", "RUNNING"),
            ("plant/line-2/status", "STOPPED"),
        ]);

        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.main.selected.as_deref(), Some("plant"));
        assert!(state.main.expanded.contains("plant"));
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.main.selected.as_deref(), Some("plant/line-1"));
        handle_key(&mut state, KeyCode::Right);
        handle_key(&mut state, KeyCode::Char('j'));
        assert_eq!(state.main.selected.as_deref(), Some("plant/line-1/status"));
        assert_eq!(
            state.topics.latest("plant/line-1/status"),
            Some(&b"RUNNING"[..])
        );
        handle_key(&mut state, KeyCode::Left);
        assert_eq!(state.main.selected.as_deref(), Some("plant/line-1"));
        handle_key(&mut state, KeyCode::Left);
        assert!(!state.main.expanded.contains("plant/line-1"));

        // New topics arriving elsewhere must not move the selection.
        state.apply_broker_event(message("zzz/new", "1"));
        handle_key(&mut state, KeyCode::Char('k'));
        handle_key(&mut state, KeyCode::Char('j'));
        assert_eq!(state.main.selected.as_deref(), Some("plant/line-1"));
    }

    #[test]
    fn enter_on_a_leaf_focuses_the_inspector_and_tab_cycles_panels() {
        let (mut state, _directory) = state_with(&[("status", "RUNNING")]);
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.main.focus, Panel::Inspector);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::Topics);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(state.main.focus, Panel::History);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::Topics);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(
            state.main.focus,
            Panel::Inspector,
            "hidden history is skipped"
        );
    }

    #[test]
    fn search_filters_topics_without_touching_the_data_and_esc_restores_them() {
        let (mut state, _directory) = state_with(&[
            ("plant/line-1/status", "RUNNING"),
            ("plant/line-2/status", "STOPPED"),
            ("other/value", "1"),
        ]);
        handle_key(&mut state, KeyCode::Char('/'));
        press(&mut state, "LINE-2");
        handle_key(&mut state, KeyCode::Enter);
        let paths: Vec<_> = state
            .main
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(paths, ["plant", "plant/line-2", "plant/line-2/status"]);
        assert_eq!(state.topics.topic_count(), 3);
        assert!(screen(&mut state, 120, 20).contains("/ LINE-2"));

        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.main.filter_active());
        assert_eq!(state.main.rows.len(), 2);
    }

    #[test]
    fn search_supports_fuzzy_matching_and_keeps_a_visible_selection() {
        let (mut state, _directory) = state_with(&[("press1/service", "1"), ("zeta/x", "2")]);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.main.selected.as_deref(), Some("zeta"));
        handle_key(&mut state, KeyCode::Char('/'));
        press(&mut state, "psv");
        let selected = state.main.selected.clone();
        assert_eq!(
            selected.as_deref(),
            Some("press1"),
            "falls back to a visible row"
        );
        assert!(
            state
                .main
                .rows
                .iter()
                .any(|row| row.path == "press1/service")
        );
    }

    #[test]
    fn maximizing_preserves_selection_and_follows_focus() {
        let (mut state, _directory) = state_with(&[("a", "1"), ("b", "2")]);
        handle_key(&mut state, KeyCode::Down);
        press(&mut state, "m");
        assert_eq!(state.main.layout.maximized, Some(Panel::Topics));
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("TOPICS") && !text.contains("PAYLOAD INSPECTOR"));
        assert_eq!(state.main.selected.as_deref(), Some("b"));

        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.layout.maximized, Some(Panel::Inspector));
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("PAYLOAD INSPECTOR") && !text.contains("TOPICS"));
        assert!(text.contains("MATE v") && !text.contains("[Esc]"));

        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.main.layout.maximized, None);
        assert_eq!(state.main.selected.as_deref(), Some("b"));
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("TOPICS") && text.contains("PAYLOAD INSPECTOR"));
    }

    #[test]
    fn default_layout_has_two_panes_and_history_is_toggled_with_h() {
        let (mut state, _directory) = state_with(&[("status", "RUNNING")]);
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("TOPICS") && text.contains("PAYLOAD INSPECTOR"));
        assert!(!text.contains("HISTORY"));

        press(&mut state, "h");
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("HISTORY") && text.contains("chg"), "{text}");
        let text = screen(&mut state, 200, 24);
        assert!(text.contains("Changed"), "{text}");
        press(&mut state, "h");
        assert!(!screen(&mut state, 140, 24).contains("HISTORY"));
    }

    #[test]
    fn narrow_terminals_show_one_pane_at_a_time() {
        let (mut state, _directory) = state_with(&[("status", "RUNNING")]);
        let text = screen(&mut state, 50, 14);
        assert!(text.contains("TOPICS") && !text.contains("PAYLOAD INSPECTOR"));
        handle_key(&mut state, KeyCode::Tab);
        let text = screen(&mut state, 50, 14);
        assert!(text.contains("PAYLOAD INSPECTOR") && !text.contains("TOPICS"));
        // Even tiny terminals must not panic.
        for (width, height) in [(20, 6), (10, 3), (1, 1), (80, 4)] {
            screen(&mut state, width, height);
        }
    }

    #[test]
    fn resizing_the_terminal_keeps_the_selection() {
        let (mut state, _directory) = state_with(&[("a", "1"), ("b", "2"), ("c", "3")]);
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Down);
        for (width, height) in [(120, 30), (45, 8), (200, 50), (120, 30)] {
            screen(&mut state, width, height);
            assert_eq!(state.main.selected.as_deref(), Some("c"));
        }
    }

    #[test]
    fn panel_width_shortcuts_work_without_ctrl_arrows() {
        let (mut state, _directory) = state_with(&[("status", "1")]);
        screen(&mut state, 120, 20);
        let before = state.main.layout.topics_two_pane;
        press(&mut state, ">");
        assert_eq!(state.main.layout.topics_two_pane, before + 5);
        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        );
        assert_eq!(state.main.layout.topics_two_pane, before);
        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
        );
        press(&mut state, "<");
        assert_eq!(state.main.layout.topics_two_pane, before);
    }

    #[test]
    fn topic_rows_show_compact_previews_instead_of_inline_json() {
        let (mut state, _directory) = state_with(&[
            ("m/status", "RUNNING"),
            ("m/object", r#"{"header":{"instance":"x"},"data":{}}"#),
            ("m/list", "[1,2,3]"),
            ("m/temp", "42.5"),
            ("m/broken", "{oops"),
        ]);
        press(&mut state, "l");
        let text = screen(&mut state, 50, 14);
        assert!(text.contains("RUNNING") && text.contains("{…}") && text.contains("[3 items]"));
        assert!(text.contains("42.5") && text.contains("!"));
        assert!(
            !text.contains("header"),
            "JSON bodies stay out of the tree:\n{text}"
        );
        for line in text.lines().skip(1).take(10) {
            assert!(line.chars().count() <= 50);
        }
    }

    #[test]
    fn long_previews_are_truncated_inside_the_pane() {
        let long = format!("\"{}\"", "x".repeat(300));
        let (mut state, _directory) = state_with(&[("topic", &long)]);
        let text = screen(&mut state, 40, 10);
        let row = text.lines().find(|line| line.contains("topic")).unwrap();
        assert!(row.contains('…'));
        assert!(
            row.trim_end().ends_with('│'),
            "preview must not cross the border: {row}"
        );
    }

    #[test]
    fn inspector_tree_expands_collapses_and_survives_new_messages() {
        let (mut state, _directory) = state_with(&[(
            "status",
            r#"{"data":{"index":33,"state":1},"header":{"sequencenr":6}}"#,
        )]);
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 120, 24);
        assert!(text.contains("▼ root") || text.contains("▼ data"), "{text}");
        assert!(text.contains("index") && text.contains("33") && text.contains("sequencenr"));
        assert!(text.contains("$"), "JSON path is shown");

        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Left);
        let text = screen(&mut state, 120, 24);
        assert!(text.contains("▶ data") && !text.contains("index"));

        state.apply_broker_event(message(
            "status",
            r#"{"data":{"index":34,"state":1},"header":{"sequencenr":7}}"#,
        ));
        let text = screen(&mut state, 120, 24);
        assert!(
            text.contains("▶ data") && !text.contains("index"),
            "collapse state survives"
        );
        assert!(text.contains("sequencenr") && text.contains('7'));
    }

    #[test]
    fn inspector_handles_invalid_json_and_binary_payloads() {
        let (mut state, _directory) = state_with(&[("bad", "{not json")]);
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("{not json") && text.contains("not valid JSON"));

        state.apply_broker_event(message_with("bad", &[0xde, 0xad, 0xbe, 0xef], false));
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("de ad be ef") && text.contains("Hex"),
            "{text}"
        );
        press(&mut state, "r");
        screen(&mut state, 120, 20);
    }

    #[test]
    fn inspector_toggles_raw_mode_searches_and_copies() {
        let (mut state, _directory) = state_with(&[(
            "status",
            r#"{"data":{"name":"Trigger"},"header":{"seq":6}}"#,
        )]);
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "r");
        let text = screen(&mut state, 120, 24);
        assert!(text.contains("Raw") && text.contains("\"header\": {"));

        press(&mut state, "r/trig");
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 120, 24);
        assert!(text.contains("1/1"), "{text}");

        press(&mut state, "C");
        let request = state.take_clipboard_request().unwrap();
        assert_eq!(request.what, "payload");
        assert!(request.text.contains("Trigger"));
        assert!(state.take_clipboard_request().is_none());
    }

    #[test]
    fn copying_the_selected_json_value_uses_the_value_under_the_cursor() {
        let (mut state, _directory) = state_with(&[("status", r#"{"name":"Trigger"}"#)]);
        handle_key(&mut state, KeyCode::Enter);
        screen(&mut state, 120, 20);
        handle_key(&mut state, KeyCode::Down);
        press(&mut state, "c");
        let request = state.take_clipboard_request().unwrap();
        assert_eq!(
            (request.text.as_str(), request.what.as_str()),
            ("Trigger", "value")
        );
    }

    #[test]
    fn topics_and_payloads_can_be_copied_from_every_panel() {
        let (mut state, _directory) =
            state_with(&[("plant/line1", "first"), ("plant/line1", "second")]);
        // Expand "plant" and select the leaf.
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.main.selected.as_deref(), Some("plant/line1"));

        press(&mut state, "t");
        let request = state.take_clipboard_request().unwrap();
        assert_eq!(
            (request.text.as_str(), request.what.as_str()),
            ("plant/line1", "topic")
        );

        // Topic panel: `c` copies the payload without opening the inspector first.
        press(&mut state, "c");
        assert_eq!(state.take_clipboard_request().unwrap().text, "second");

        // History panel: the message under the history cursor.
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::History);
        screen(&mut state, 120, 30);
        handle_key(&mut state, KeyCode::Down);
        press(&mut state, "c");
        assert_eq!(state.take_clipboard_request().unwrap().text, "first");
    }

    #[test]
    fn ctrl_c_copies_instead_of_quitting_and_ctrl_q_quits() {
        let (mut state, _directory) = state_with(&[("status", "hello")]);
        let ctrl = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);

        let action = handle_key_event(&mut state, ctrl('c'));
        assert!(matches!(action, UiAction::None));
        assert_eq!(state.take_clipboard_request().unwrap().text, "hello");

        // Dialogs and text fields do not copy and never quit.
        state.open_broker_editor(None);
        let action = handle_key_event(&mut state, ctrl('c'));
        assert!(matches!(action, UiAction::None));
        assert!(state.take_clipboard_request().is_none());
        handle_key(&mut state, KeyCode::Esc);

        let action = handle_key_event(&mut state, ctrl('q'));
        assert!(matches!(action, UiAction::None), "quitting asks first");
        assert!(matches!(state.confirmation, Some(Confirmation::Quit)));
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('y')),
            UiAction::Quit
        ));
    }

    #[test]
    fn history_selection_stays_stable_while_new_messages_arrive() {
        let (mut state, _directory) =
            state_with(&[("status", "1"), ("status", "2"), ("status", "3")]);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::History);

        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Enter);
        let pinned = state.main.pinned.expect("message pinned");
        screen(&mut state, 140, 24);
        assert_eq!(&*state.displayed_message().unwrap().payload, b"2");
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("HISTORY –"), "{text}");

        state.apply_broker_event(message("status", "4"));
        state.apply_broker_event(message("status", "5"));
        screen(&mut state, 140, 24);
        assert_eq!(state.main.pinned, Some(pinned));
        assert_eq!(&*state.displayed_message().unwrap().payload, b"2");
        assert_eq!(
            state.topics.latest("status"),
            Some(&b"5"[..]),
            "reception continues"
        );

        // The cursor also stays on its message instead of drifting with new arrivals.
        assert_eq!(
            state
                .topics
                .history("status")
                .unwrap()
                .iter()
                .rev()
                .nth(state.history_cursor_index())
                .unwrap()
                .seq,
            pinned
        );

        press(&mut state, "l");
        assert_eq!(state.main.pinned, None);
        screen(&mut state, 140, 24);
        assert_eq!(&*state.displayed_message().unwrap().payload, b"5");
        assert!(screen(&mut state, 140, 24).contains("LIVE"));
    }

    #[test]
    fn leaving_history_with_esc_returns_to_live_and_selection_change_resets_it() {
        let (mut state, _directory) = state_with(&[("a", "1"), ("a", "2"), ("b", "x")]);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.main.pinned.is_some());
        handle_key(&mut state, KeyCode::Esc);
        assert!(state.main.pinned.is_none());

        handle_key(&mut state, KeyCode::Enter);
        assert!(state.main.pinned.is_some());
        state.main.focus = Panel::Topics;
        handle_key(&mut state, KeyCode::Down);
        assert!(
            state.main.pinned.is_none(),
            "another topic always starts live"
        );
    }

    #[test]
    fn evicted_pinned_messages_fall_back_to_live_with_a_notice() {
        let (mut state, _directory) = state_with(&[("t", "0")]);
        screen(&mut state, 140, 24);
        state.main.pinned = Some(state.topics.latest_message("t").unwrap().seq);
        for index in 1..=mate::topics::MAX_HISTORY_PER_TOPIC + 1 {
            state.apply_broker_event(message("t", &index.to_string()));
        }
        screen(&mut state, 140, 24);
        assert_eq!(state.main.pinned, None);
        assert!(state.notice.is_some());
    }

    #[test]
    fn history_compare_marks_a_base_and_opens_a_diff() {
        let (mut state, _directory) = state_with(&[
            ("t", r#"{"a":1}"#),
            ("t", r#"{"a":2}"#),
            ("t", r#"{"a":3}"#),
        ]);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "d");
        assert!(state.main.compare_base.is_some());
        let text = screen(&mut state, 140, 24);
        assert!(text.contains('◆'), "{text}");

        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Down);
        press(&mut state, "d");
        assert!(state.main.compare_base.is_none());
        let diff = state.main.inspector.diff.as_ref().expect("diff opened");
        let lines = diff.lines.as_ref().unwrap();
        assert!(lines.contains(&mate::diff::DiffLine::Removed("  \"a\": 1".into())));
        assert!(lines.contains(&mate::diff::DiffLine::Added("  \"a\": 3".into())));
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("DIFF") && text.contains("- ") && text.contains("+ "));

        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Esc);
        assert!(state.main.inspector.diff.is_none());
    }

    #[test]
    fn inspector_diff_compares_with_the_previous_message() {
        let (mut state, _directory) = state_with(&[("t", "old"), ("t", "new")]);
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "d");
        let diff = state.main.inspector.diff.as_ref().expect("diff opened");
        assert!(
            diff.lines
                .as_ref()
                .unwrap()
                .contains(&mate::diff::DiffLine::Added("new".into()))
        );

        let (mut single, _directory) = state_with(&[("t", "only")]);
        handle_key(&mut single, KeyCode::Enter);
        press(&mut single, "d");
        assert!(single.main.inspector.diff.is_none() && single.notice.is_some());
    }

    #[test]
    fn shortcut_bar_depends_on_focus_and_dialogs() {
        let (mut state, _directory) = state_with(&[("status", "1")]);
        let text = screen(&mut state, 140, 20);
        assert!(text.contains("[/] Search") && text.contains("[*] Favorite"));
        assert!(text.contains("[c] Copy payload") && !text.contains("Publish"));

        handle_key(&mut state, KeyCode::Tab);
        let text = screen(&mut state, 140, 20);
        assert!(
            text.contains("[c] Copy") && text.contains("[r] Raw") && !text.contains("[*] Favorite")
        );

        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::BackTab);
        let text = screen(&mut state, 140, 20);
        assert!(text.contains("[l] Live") && text.contains("[Enter] Inspect"));

        press(&mut state, "ba");
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("[Tab] Next field") || text.contains("Next field"));
        assert!(!text.contains("[l] Live"));
    }

    #[test]
    fn favorites_toggle_and_are_marked_in_the_tree() {
        let (mut state, _directory) = state_with(&[("a", "1"), ("b", "2")]);
        press(&mut state, "*");
        assert!(state.main.favorites.contains("a"));
        let text = screen(&mut state, 80, 12);
        assert!(text.contains("a *"), "{text}");
        press(&mut state, "*");
        assert!(state.main.favorites.is_empty());
    }

    #[test]
    fn broker_table_shows_status_only_for_the_connected_broker() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let _development = profile(&store, "Development");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        handle_key(&mut state, KeyCode::Char('b'));
        let text = screen(&mut state, 120, 30);
        let production_row = text
            .lines()
            .find(|l| l.contains("production.example.test"))
            .unwrap();
        let development_row = text
            .lines()
            .find(|l| l.contains("development.example.test"))
            .unwrap();
        assert!(production_row.contains("Connected"), "{text}");
        assert!(development_row.contains("Disconnected"), "{text}");
        assert!(text.contains("NAME") && text.contains("TLS") && text.contains("STATUS"));
    }

    #[test]
    fn broker_manager_connect_and_test_actions_carry_the_selected_profile() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        store
            .update(&production.id, |item| item.unique_client_id = false)
            .unwrap();
        let mut state = UiState::new(store.load().unwrap(), store, None);
        handle_key(&mut state, KeyCode::Char('b'));

        match handle_key(&mut state, KeyCode::Char('t')) {
            UiAction::TestBroker(profile) => assert_eq!(profile.id, production.id),
            _ => panic!("expected a test action"),
        }
        assert_eq!(
            state.test_results.get(&production.id),
            Some(&TestOutcome::Running)
        );
        state.apply_test_result(&production.id, Err("refused".into()));
        assert!(screen(&mut state, 120, 30).contains("Test failed"));

        // Testing the active broker must not reuse a fixed client ID.
        connected(&mut state, &production);
        match handle_key(&mut state, KeyCode::Char('t')) {
            UiAction::TestBroker(profile) => assert!(profile.client_id.ends_with("-test")),
            _ => panic!("expected a test action"),
        }

        match handle_key(&mut state, KeyCode::Enter) {
            UiAction::Connect(profile) => assert_eq!(profile.id, production.id),
            _ => panic!("expected a connect action"),
        }
        assert_eq!(state.view, View::Main);
    }

    #[test]
    fn broker_manager_duplicates_profiles_and_toggles_the_default() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        handle_key(&mut state, KeyCode::Char('b'));

        handle_key(&mut state, KeyCode::Char('*'));
        assert_eq!(
            store.load().unwrap().general.default_broker.as_deref(),
            Some(production.id.as_str())
        );
        assert!(screen(&mut state, 120, 30).contains("Production (default)"));

        handle_key(&mut state, KeyCode::Char('u'));
        let names: Vec<_> = state
            .config
            .brokers
            .iter()
            .map(|profile| profile.name.as_str())
            .collect();
        assert_eq!(names, ["Production", "Production copy"]);
        assert_eq!(
            state.config.brokers[state.broker_selection].name,
            "Production copy"
        );
        assert_eq!(store.list_profiles().unwrap().len(), 2);
        let text = screen(&mut state, 120, 30);
        assert!(text.contains("Production copy") && text.contains("Duplicated as"));

        handle_key(&mut state, KeyCode::Up);
        handle_key(&mut state, KeyCode::Char('*'));
        assert_eq!(store.load().unwrap().general.default_broker, None);
        assert!(!screen(&mut state, 120, 30).contains("(default)"));
    }

    #[test]
    fn connect_without_a_selection_opens_the_broker_list() {
        let (store, _directory) = temp_store();
        profile(&store, "One");
        profile(&store, "Two");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('o')),
            UiAction::None
        ));
        assert_eq!(state.view, View::Brokers);

        let (store, _directory) = temp_store();
        let only = profile(&store, "Only");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        match handle_key(&mut state, KeyCode::Char('o')) {
            UiAction::Connect(profile) => assert_eq!(profile.id, only.id),
            _ => panic!("a single profile connects directly"),
        }
    }

    #[test]
    fn redraws_happen_only_when_something_changed() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store, None);
        let start = state.now;
        assert!(state.take_redraw());
        assert!(!state.take_redraw());
        state.tick(start + Duration::from_secs(10));
        assert!(
            !state.take_redraw(),
            "an idle, empty UI has nothing to animate"
        );

        let (mut live, _directory) = state_with(&[("a", "1")]);
        assert!(live.take_redraw());
        let now = live.now;
        live.tick(now + Duration::from_millis(600));
        assert!(
            live.take_redraw(),
            "activity indicators and rates age with time"
        );
        live.tick(now + Duration::from_millis(700));
        assert!(!live.take_redraw());
        live.apply_broker_event(message("a", "2"));
        assert!(live.take_redraw());
    }

    #[test]
    fn notices_expire() {
        let (mut state, _directory) = state_with(&[]);
        state.notify("hello", false);
        let now = state.now;
        state.tick(now + NOTICE_LIFETIME + Duration::from_secs(1));
        assert!(state.notice.is_none());
    }

    #[test]
    fn help_overlay_documents_the_alternative_width_shortcuts() {
        let (mut state, _directory) = state_with(&[]);
        press(&mut state, "?");
        let text = screen(&mut state, 140, 50);
        assert!(text.contains("HELP") && text.contains("Ctrl+←/→") && text.contains("< >"));
        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.help_open);
    }

    #[test]
    fn realistic_plc_payload_renders_as_a_compact_tree_row_and_expandable_inspector() {
        let payload = r#"{"header":{"creationtime":"2026-10-21T09:14:15.6410000Z","instancename":"PRESS1_TriggerService","repetition":0,"sequence":6},"data":{"additional":"MQTTClient State: MQTT_ERR_CONN","index":33,"state":1,"path":"PRG_MAIN.fb_Coordinator.fb_Mqtt.fb_Connect.fb_ClientError"}}"#;
        let (mut state, _directory) = state_with(&[
            ("site1/hmi/message/PRESS1_TriggerService", payload),
            ("tower1/machines/machine-01/status", "RUNNING"),
        ]);
        press(&mut state, "ljljlj");
        handle_key(&mut state, KeyCode::Enter);

        let text = screen(&mut state, 120, 30);
        let topic_row = text
            .lines()
            .find(|line| line.contains("PRESS1_TriggerService  {…}"))
            .unwrap_or_else(|| panic!("compact row missing:\n{text}"));
        assert!(topic_row.contains('●'));
        for expected in [
            "▼ site1",
            "▼ hmi",
            "▼ root",
            "creationtime",
            "MQTT_ERR_CONN",
            "msg 1 of 1",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        assert!(text.contains("LIVE"));
    }

    #[test]
    fn control_characters_in_topics_and_payloads_never_reach_the_terminal() {
        let (mut state, _directory) = state_with(&[
            (
                "evil/\u{1b}[2Jtopic",
                "plain \u{1b}[31mred\u{7} text\twith tab",
            ),
            ("evil/json", "{\"k\":\"\u{1b}[31mv\"}"),
        ]);
        press(&mut state, "lj");
        handle_key(&mut state, KeyCode::Enter);
        for width in [60, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| render(frame, &mut state)).unwrap();
            let buffer = terminal.backend().buffer();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    assert!(
                        !buffer[(x, y)].symbol().chars().any(char::is_control),
                        "control character in cell ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_is_restored_from_the_saved_ui_state() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);
        press(&mut state, "h>");
        let saved_width = state.main.layout.topics_two_pane;
        assert!(store.root().join("ui-state.toml").is_file());

        let restored = UiState::new(AppConfig::default(), store, None);
        assert!(restored.main.layout.show_history);
        assert_eq!(restored.main.layout.topics_two_pane, saved_width);
        assert!(
            restored.main.layout.maximized.is_none(),
            "maximizing is not persisted"
        );
    }

    #[test]
    fn nothing_is_written_while_the_defaults_are_unchanged() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);
        press(&mut state, "jjkm");
        assert!(!store.root().join("ui-state.toml").exists());
    }

    #[test]
    fn favorites_are_saved_per_broker_and_restored_on_connect() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let development = profile(&store, "Development");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        connected(&mut state, &production);
        state.apply_broker_event(message("plant/status", "1"));
        press(&mut state, "*");
        assert!(state.main.favorites.contains("plant"));

        let mut again = UiState::new(store.load().unwrap(), store.clone(), None);
        connected(&mut again, &development);
        assert!(
            again.main.favorites.is_empty(),
            "other brokers have their own favorites"
        );
        connected(&mut again, &production);
        assert!(again.main.favorites.contains("plant"));

        press(&mut state, "*");
        let cleared = UiSettings::load(store.root()).unwrap();
        assert!(!cleared.favorites.contains_key(&production.id));
    }

    #[test]
    fn favorites_filter_shows_only_favorite_subtrees_and_explains_an_empty_list() {
        let (mut state, _directory) = state_with(&[
            ("plant/line-1/status", "RUNNING"),
            ("plant/line-2/status", "STOPPED"),
            ("other/value", "1"),
        ]);
        press(&mut state, "F");
        assert!(state.main.rows.is_empty());
        assert!(screen(&mut state, 100, 20).contains("No favorites yet"));
        press(&mut state, "F");
        assert_eq!(state.main.rows.len(), 2);

        // Select "plant" (second root) and mark it: every topic below it counts.
        press(&mut state, "j*F");
        let paths: Vec<_> = state
            .main
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "plant",
                "plant/line-1",
                "plant/line-1/status",
                "plant/line-2",
                "plant/line-2/status"
            ]
        );
        assert!(screen(&mut state, 100, 20).contains("[favorites]"));
        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.main.favorites_only);
        assert_eq!(state.main.rows.len(), 2);
    }

    #[test]
    fn unreadable_or_unwritable_ui_state_is_reported_without_blocking_the_ui() {
        let (store, _directory) = temp_store();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.root().join("ui-state.toml"), "layout = [").unwrap();
        let mut state = UiState::new(AppConfig::default(), store.clone(), None);
        assert!(state.notice.as_ref().is_some_and(|notice| notice.error));
        assert_eq!(state.main.layout.topics_two_pane, 35);

        // A directory in the way makes every save fail; the UI keeps working.
        std::fs::remove_file(store.root().join("ui-state.toml")).unwrap();
        std::fs::create_dir(store.root().join("ui-state.toml")).unwrap();
        state.notice = None;
        press(&mut state, "h");
        assert!(state.main.layout.show_history);
        let notice = state.notice.as_ref().expect("failure is shown");
        assert!(notice.error && notice.text.contains("not saved"));
        state.notice = None;
        press(&mut state, "h");
        assert!(state.notice.is_none(), "the failure is reported only once");
    }

    fn writable_state(topics: &[(&str, &str)]) -> (UiState, tempfile::TempDir) {
        let (store, directory) = temp_store();
        let mut production = profile(&store, "Production");
        production.allow_publish = true;
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        for (topic, payload) in topics {
            state.apply_broker_event(message(topic, payload));
        }
        (state, directory)
    }

    #[test]
    fn publishing_is_refused_for_read_only_brokers_and_without_a_connection() {
        let (mut state, _directory) = state_with(&[("cmd", "1")]);
        press(&mut state, "p");
        assert!(state.publish_editor.is_none());
        let notice = state.notice.as_ref().expect("explained");
        assert!(notice.error && notice.text.contains("Publishing is off"));
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("read-only") && !text.contains("[p] Publish"));

        let (store, _directory) = temp_store();
        let mut idle = UiState::new(AppConfig::default(), store, None);
        press(&mut idle, "p");
        assert!(idle.publish_editor.is_none());
        assert!(idle.notice.as_ref().unwrap().text.contains("Connect"));
    }

    #[test]
    fn publish_dialog_reviews_before_it_returns_a_request() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        let text = screen(&mut state, 140, 24);
        assert!(text.contains("publish on") && text.contains("[p] Publish"));

        press(&mut state, "p");
        assert_eq!(state.publish_editor.as_ref().unwrap().topic, "cmd");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "{\"a\":1}");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "1");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, " ");
        let text = screen(&mut state, 140, 28);
        assert!(text.contains("PUBLISH") && text.contains("[1]") && text.contains("[x] yes"));

        assert!(matches!(
            handle_key(&mut state, KeyCode::Enter),
            UiAction::None
        ));
        let text = screen(&mut state, 140, 28);
        assert!(
            text.contains("REVIEW PUBLISH") && text.contains("Retained:"),
            "{text}"
        );
        assert!(text.contains("7 bytes") && text.contains("QoS 1, retained"));

        // Going back keeps what was typed; nothing was sent.
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('n')),
            UiAction::None
        ));
        assert_eq!(state.publish_editor.as_ref().unwrap().payload, "{\"a\":1}");
        handle_key(&mut state, KeyCode::Enter);
        match handle_key(&mut state, KeyCode::Char('y')) {
            UiAction::Publish(_, request) => {
                assert_eq!(request.topic, "cmd");
                assert_eq!(request.payload, b"{\"a\":1}");
                assert_eq!((request.qos, request.retain), (1, true));
            }
            _ => panic!("expected a publish action"),
        }
        assert!(state.publish_editor.is_none());
    }

    #[test]
    fn publish_dialog_lists_earlier_messages_below_the_fields_for_editing() {
        let request = |topic: &str, payload: &str| PublishRequest {
            topic: topic.to_owned(),
            payload: payload.as_bytes().to_vec(),
            qos: 1,
            retain: false,
            user_properties: Vec::new(),
        };
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);

        press(&mut state, "p");
        assert!(!screen(&mut state, 120, 36).contains("Recent messages"));
        // Without history the cycle has only the four fields.
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_RETAIN
        );
        handle_key(&mut state, KeyCode::Esc);

        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request("cmd", r#"{"a":1}"#),
            Ok(()),
        );
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request("other", "x"),
            Ok(()),
        );
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request("failed", "y"),
            Err("not connected".into()),
        );
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request("cmd", r#"{"a":1}"#),
            Ok(()),
        );
        let topics: Vec<_> = state
            .publish_history()
            .iter()
            .map(|e| e.topic.as_str())
            .collect();
        assert_eq!(
            topics,
            ["cmd", "other"],
            "newest first, no duplicates, no failures"
        );

        press(&mut state, "p");
        let text = screen(&mut state, 120, 36);
        assert!(
            text.contains("Recent messages") && text.contains("other"),
            "{text}"
        );

        // Down from the last field enters the list; Down again selects the older message.
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Down);
        }
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_RECENT
        );
        handle_key(&mut state, KeyCode::Down);
        let text = screen(&mut state, 120, 36);
        assert!(
            text.contains("> ") && text.contains("Enter loads one"),
            "{text}"
        );
        handle_key(&mut state, KeyCode::Enter);
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(
            (editor.topic.as_str(), editor.payload.as_str()),
            ("other", "x")
        );
        assert_eq!(editor.field, publish::FIELD_PAYLOAD);

        // The loaded message is edited and goes through the normal review.
        press(&mut state, "yz");
        for _ in 0..3 {
            handle_key(&mut state, KeyCode::Enter);
        }
        match handle_key(&mut state, KeyCode::Char('y')) {
            UiAction::Publish(_, sent) => assert_eq!(sent.payload, b"xyz"),
            _ => panic!("expected a publish action"),
        }

        let store = state.broker_store.clone();
        let reloaded = UiState::new(store.load().unwrap(), store, None);
        assert_eq!(reloaded.publish_history.values().next().unwrap().len(), 2);

        press(&mut state, "p");
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_RECENT
        );
        handle_key(&mut state, KeyCode::Char('d'));
        assert_eq!(state.publish_history().len(), 1);
        handle_key(&mut state, KeyCode::Char('d'));
        assert!(state.publish_history().is_empty());
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_PAYLOAD
        );
    }

    #[test]
    fn payload_field_shows_several_lines_and_edits_at_the_cursor() {
        let ctrl = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);

        press(&mut state, "{\"a\":1,\"b\":2}");
        handle_key_event(&mut state, ctrl('f'));
        let text = screen(&mut state, 120, 36);
        for expected in ["\"a\": 1,", "\"b\": 2", "line 4/4", "[Ctrl+F] JSON layout"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }

        // Move up two lines to the "a" line, jump to its end and insert a new key.
        handle_key(&mut state, KeyCode::Up);
        handle_key(&mut state, KeyCode::Up);
        handle_key(&mut state, KeyCode::End);
        handle_key(&mut state, KeyCode::Left);
        press(&mut state, "0");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.payload, "{\n  \"a\": 10,\n  \"b\": 2\n}");
        assert_eq!(
            editor.field,
            publish::FIELD_PAYLOAD,
            "Up stayed inside the payload"
        );

        // Alt+Enter and Ctrl+J insert a line break, Enter still moves on.
        handle_key_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        handle_key_event(&mut state, ctrl('j'));
        assert_eq!(
            state
                .publish_editor
                .as_ref()
                .unwrap()
                .payload
                .matches('\n')
                .count(),
            5
        );
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().field,
            publish::FIELD_QOS
        );

        // The review shows the payload lines, not just the first one.
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 120, 36);
        assert!(
            text.contains("REVIEW PUBLISH") && text.contains("\"b\": 2"),
            "{text}"
        );
        handle_key(&mut state, KeyCode::Esc);

        handle_key(&mut state, KeyCode::BackTab);
        handle_key(&mut state, KeyCode::BackTab);
        handle_key_event(&mut state, ctrl('u'));
        assert!(state.publish_editor.as_ref().unwrap().payload.is_empty());
        handle_key_event(&mut state, ctrl('f'));
        assert!(
            state
                .publish_editor
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("Not valid JSON")
        );
    }

    #[test]
    fn ctrl_s_sends_without_the_review_but_not_mistakes() {
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);

        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "{\"a\":1}");
        match handle_key_event(&mut state, ctrl(KeyCode::Char('s'))) {
            UiAction::Publish(_, request) => {
                assert_eq!(
                    (request.topic.as_str(), request.payload.as_slice()),
                    ("cmd", &b"{\"a\":1}"[..])
                );
            }
            _ => panic!("expected an immediate publish"),
        }
        assert!(state.publish_editor.is_none());

        // An invalid topic is reported in the dialog and nothing is sent.
        press(&mut state, "p");
        press(&mut state, "/+");
        assert!(matches!(
            handle_key_event(&mut state, ctrl(KeyCode::Char('s'))),
            UiAction::None
        ));
        assert!(state.publish_editor.as_ref().unwrap().error.is_some());
        handle_key(&mut state, KeyCode::Esc);

        // Deleting a retained message and broken JSON still go through the review.
        press(&mut state, "p");
        for _ in 0..3 {
            handle_key(&mut state, KeyCode::Tab);
        }
        press(&mut state, "y");
        assert!(matches!(
            handle_key_event(&mut state, ctrl(KeyCode::Char('s'))),
            UiAction::None
        ));
        assert_eq!(
            state.publish_editor.as_ref().unwrap().stage,
            publish::PublishStage::Review
        );
        assert!(state.notice.as_ref().unwrap().text.contains("review first"));
        // In the review Ctrl+S confirms, like y.
        assert!(matches!(
            handle_key_event(&mut state, ctrl(KeyCode::Char('s'))),
            UiAction::Publish(..)
        ));
    }

    #[test]
    fn topic_field_has_a_free_cursor() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Home);
        press(&mut state, "x/");
        handle_key(&mut state, KeyCode::End);
        press(&mut state, "/y");
        handle_key(&mut state, KeyCode::Left);
        handle_key(&mut state, KeyCode::Left);
        handle_key(&mut state, KeyCode::Backspace);
        handle_key(&mut state, KeyCode::Delete);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().topic,
            "x/cm/y".replace("cm/y", "cmy")
        );
    }

    #[test]
    fn publish_dialog_rejects_invalid_topics_and_can_be_cancelled() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        press(&mut state, "/+");
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Enter);
        }
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.stage, publish::PublishStage::Edit);
        assert!(editor.error.as_deref().unwrap().contains("wildcards"));
        assert!(screen(&mut state, 140, 28).contains("wildcards"));

        handle_key(&mut state, KeyCode::Esc);
        assert!(state.publish_editor.is_none());
        assert!(!state.is_plain_main_view() || state.main.layout.maximized.is_none());
    }

    #[test]
    fn publish_review_warns_about_deleting_retained_messages() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        for _ in 0..3 {
            handle_key(&mut state, KeyCode::Tab);
        }
        press(&mut state, "y");
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 140, 28);
        assert!(text.contains("DELETES the retained message"), "{text}");
    }

    #[test]
    fn pasting_fills_the_focused_field_without_triggering_shortcuts() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_paste(&mut state, "plant/line-1\n/cmd\n");
        handle_key(&mut state, KeyCode::Tab);
        handle_paste(&mut state, "{\n  \"a\": 1\n}");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.topic, "cmdplant/line-1 /cmd");
        assert_eq!(editor.payload, "{\n  \"a\": 1\n}");
        let text = screen(&mut state, 140, 28);
        assert!(
            text.contains("\"a\": 1") && text.contains("line 3/3"),
            "{text}"
        );

        // Pasting into the broker editor goes to the focused field, digits only for the port.
        let (store, _directory) = temp_store();
        let mut other = UiState::new(AppConfig::default(), store, None);
        press(&mut other, "ba");
        handle_paste(&mut other, "My Broker\n");
        handle_key(&mut other, KeyCode::Tab);
        handle_key(&mut other, KeyCode::Tab);
        handle_paste(&mut other, " 18\n83x");
        let editor = other.editor.as_ref().unwrap();
        assert_eq!(editor.name, "My Broker ");
        assert_eq!(
            editor.port, "18831883",
            "digits are appended, everything else dropped"
        );
    }

    #[test]
    fn publish_results_are_reported_in_the_notice() {
        let (mut state, _directory) = writable_state(&[]);
        let request = mate::mqtt::PublishRequest {
            topic: "a/b".to_owned(),
            payload: b"hi".to_vec(),
            qos: 1,
            retain: true,
            user_properties: Vec::new(),
        };
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request,
            Ok(()),
        );
        let text = state.notice.as_ref().unwrap().text.clone();
        assert!(text.contains("Published 2 bytes to a/b") && text.contains("retained"));
        let clear = mate::mqtt::PublishRequest {
            payload: Vec::new(),
            ..request.clone()
        };
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &clear,
            Ok(()),
        );
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Cleared the retained")
        );
        state.report_publish(
            &state.default_publish_broker().unwrap_or_default(),
            &request,
            Err("not connected".into()),
        );
        let notice = state.notice.as_ref().unwrap();
        assert!(notice.error && notice.text.contains("Not published"));
    }

    #[test]
    fn broker_manager_toggles_the_publish_permission_per_profile() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        handle_key(&mut state, KeyCode::Char('b'));
        assert!(screen(&mut state, 120, 30).contains("read-only (w to allow)"));

        handle_key(&mut state, KeyCode::Char('w'));
        assert!(store.get(&production.id).unwrap().allow_publish);
        assert!(state.config.brokers[0].allow_publish);
        assert!(screen(&mut state, 120, 30).contains("allowed (w to forbid)"));
        handle_key(&mut state, KeyCode::Char('w'));
        assert!(!store.get(&production.id).unwrap().allow_publish);
    }

    #[test]
    fn high_message_volume_keeps_state_bounded() {
        let (mut state, _directory) = state_with(&[]);
        for index in 0..5000 {
            state.apply_broker_event(message("busy", &index.to_string()));
        }
        assert_eq!(
            state.topics.history("busy").unwrap().len(),
            mate::topics::MAX_HISTORY_PER_TOPIC
        );
        assert!(screen(&mut state, 140, 24).contains("4999"));
    }

    /// A busy topic, a slow one and three topics whose latest payload is not valid JSON.
    fn analytics_state() -> (UiState, tempfile::TempDir) {
        let (mut state, directory) = state_with(&[
            ("plc/slow", "1"),
            ("plc/slow", "2"),
            ("plc/status", "RUNNING"),
            ("plc/config", "{\"a\":"),
        ]);
        for cycle in 0..50 {
            state.apply_broker_event(message("plc/cycle/busy", &cycle.to_string()));
        }
        state.apply_broker_event(message_with("plc/raw", &[0xff, 0x00, 0x10], false));
        (state, directory)
    }

    fn analytics_topics(state: &UiState) -> Vec<&str> {
        state
            .analytics
            .rows
            .iter()
            .map(|row| row.topic.as_str())
            .collect()
    }

    #[test]
    fn analytics_ranks_topics_by_frequency_in_either_direction() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A");
        assert_eq!(state.view, View::Analytics);
        assert_eq!(
            analytics_topics(&state),
            [
                "plc/cycle/busy",
                "plc/slow",
                "plc/config",
                "plc/raw",
                "plc/status"
            ]
        );
        assert_eq!(state.analytics.rows[0].count, 50);
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("COUNT▼") && text.contains("plc/cycle/busy"));

        press(&mut state, "r");
        assert_eq!(analytics_topics(&state)[4], "plc/cycle/busy");
        assert!(screen(&mut state, 120, 20).contains("COUNT▲"));

        press(&mut state, "s");
        assert_eq!(state.analytics.sort, analytics::SortKey::Rate);
        assert!(screen(&mut state, 120, 20).contains("RATE▲"));
    }

    #[test]
    fn analytics_lists_topics_without_valid_json_and_filters_by_kind() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A2");
        assert_eq!(state.analytics.mode, analytics::Mode::InvalidJson);
        assert_eq!(
            analytics_topics(&state),
            ["plc/config", "plc/raw", "plc/status"],
            "valid JSON, including bare numbers, is not listed"
        );
        let text = screen(&mut state, 140, 20);
        assert!(text.contains("broken") && text.contains("binary") && text.contains("text"));
        assert!(
            text.contains("EOF"),
            "the parser message explains the problem"
        );

        press(&mut state, "f");
        assert_eq!(analytics_topics(&state), ["plc/config"]);
        assert!(screen(&mut state, 140, 20).contains("broken JSON only"));
        press(&mut state, "ff");
        assert_eq!(analytics_topics(&state), ["plc/raw"]);
        press(&mut state, "ff");
        assert_eq!(state.analytics.kind_filter, None);
        assert_eq!(state.analytics.rows.len(), 3);

        press(&mut state, "1");
        assert_eq!(state.analytics.mode, analytics::Mode::Frequency);
        assert_eq!(state.analytics.rows.len(), 5);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.analytics.mode, analytics::Mode::InvalidJson);
        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(state.analytics.mode, analytics::Mode::Frequency);
    }

    #[test]
    fn analytics_updates_classification_when_a_topic_changes() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A2");
        assert_eq!(state.analytics.rows.len(), 3);
        state.apply_broker_event(message("plc/config", "{\"a\":1}"));
        assert_eq!(state.analytics.rows.len(), 3, "the list is a snapshot");
        state.tick(state.now + Duration::from_secs(2));
        screen(&mut state, 100, 20);
        assert_eq!(analytics_topics(&state), ["plc/raw", "plc/status"]);
    }

    #[test]
    fn analytics_enter_reveals_the_topic_in_the_explorer() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.view, View::Main);
        assert_eq!(state.main.selected.as_deref(), Some("plc/cycle/busy"));
        assert!(state.main.expanded.contains("plc") && state.main.expanded.contains("plc/cycle"));
        assert_eq!(state.main.focus, Panel::Topics);
        let visible: Vec<_> = state
            .main
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert!(visible.contains(&"plc/cycle/busy"));
    }

    #[test]
    fn analytics_counts_only_the_current_connection_and_hides_idle_topics() {
        let (store, _directory) = temp_store();
        let production = profile(&store, "Production");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &production);
        state.apply_broker_event(message("a/old", "1"));
        state.begin_connection(&production);
        state.apply_broker_event(message("a/new", "1"));
        press(&mut state, "A");
        assert_eq!(analytics_topics(&state), ["a/new"]);
        assert_eq!(state.analytics.idle, 1);
        assert!(screen(&mut state, 140, 20).contains("1 cached topics without messages"));
        press(&mut state, "2");
        assert_eq!(analytics_topics(&state), Vec::<&str>::new());
    }

    #[test]
    fn analytics_view_closes_with_escape_and_quits_with_q() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A");
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.view, View::Main);
        press(&mut state, "A");
        handle_key(&mut state, KeyCode::Char('A'));
        assert_eq!(state.view, View::Main);
        press(&mut state, "A");
        let bar = screen(&mut state, 120, 20);
        assert!(bar.contains("[Tab/1-7] List") && bar.contains("[Enter] Show in tree"));
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('q')),
            UiAction::None
        ));
        assert_eq!(
            state.view,
            View::Analytics,
            "the view stays behind the question"
        );
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('y')),
            UiAction::Quit
        ));
    }

    #[test]
    fn quitting_asks_for_confirmation_in_every_view_and_can_be_cancelled() {
        let (mut state, _directory) = state_with(&[("a/b", "1")]);
        for keys in ["", "A", "S", "b"] {
            press(&mut state, keys);
            assert!(matches!(
                handle_key(&mut state, KeyCode::Char('q')),
                UiAction::None
            ));
            let text = screen(&mut state, 100, 24);
            assert!(
                text.contains("Quit MATE?") && text.contains("[y/Enter] Quit"),
                "{keys}:\n{text}"
            );
            assert!(matches!(
                handle_key(&mut state, KeyCode::Char('x')),
                UiAction::None
            ));
            assert!(state.confirmation.is_some(), "other keys do not answer");
            handle_key(&mut state, KeyCode::Esc);
            assert!(state.confirmation.is_none(), "Esc stays");
            handle_key(&mut state, KeyCode::Esc);
        }
        let action = handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, UiAction::None));
        let again = handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        );
        assert!(matches!(again, UiAction::Quit), "Ctrl+Q twice quits");

        press(&mut state, "q");
        assert!(
            matches!(handle_key(&mut state, KeyCode::Enter), UiAction::Quit),
            "Enter confirms"
        );
    }

    #[test]
    fn the_quit_question_also_covers_open_dialogs() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        let action = handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        );
        assert!(matches!(action, UiAction::None));
        assert!(screen(&mut state, 140, 40).contains("Quit MATE?"));
        handle_key(&mut state, KeyCode::Char('n'));
        assert!(state.publish_editor.is_some(), "the dialog is still open");
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('q')),
            UiAction::None
        ));
        assert_eq!(
            state.publish_editor.as_ref().unwrap().topic,
            "cmdq",
            "q is text in a dialog"
        );
    }

    fn message_at(topic: &str, payload: &[u8], retain: bool, received: Instant) -> BrokerEvent {
        BrokerEvent::Message(IncomingMessage {
            topic: topic.to_owned(),
            payload: payload.to_vec(),
            qos: 0,
            retain,
            dup: false,
            properties: None,
            received,
            wall: SystemTime::now(),
        })
    }

    /// Topics with known receive times, viewed 100 seconds after the start.
    fn timed_analytics_state() -> (UiState, tempfile::TempDir) {
        let (mut state, directory) = state_with(&[]);
        let base = Instant::now();
        let mut send = |topic: &str, payload: &[u8], retain: bool, seconds: u64| {
            let received = base + Duration::from_secs(seconds);
            state.apply_broker_event(message_at(topic, payload, retain, received));
        };
        for second in 0..=10 {
            send("line/regular", b"1", false, second);
        }
        for second in [0, 1, 3, 6, 10, 15, 21] {
            send("line/wobbly", b"abc", false, second);
        }
        for second in [0, 10, 20, 30, 40] {
            send("line/gone", b"ok", false, second);
        }
        send("line/big", &[b'x'; 5000], false, 50);
        send("line/big", b"1", false, 51);
        send("line/big", &[b'y'; 20], true, 52);
        send("line/spike", &[b's'; 100], false, 60);
        send("other/x", b"1", false, 70);
        state.tick(base + Duration::from_secs(100));
        (state, directory)
    }

    #[test]
    fn analytics_quiet_ranks_by_silence_or_by_how_overdue_a_topic_is() {
        let (mut state, _directory) = timed_analytics_state();
        press(&mut state, "A3");
        assert_eq!(state.analytics.mode, analytics::Mode::Quiet);
        assert_eq!(
            analytics_topics(&state),
            [
                "line/regular",
                "line/wobbly",
                "line/gone",
                "line/big",
                "line/spike",
                "other/x"
            ]
        );
        let text = screen(&mut state, 120, 20);
        assert!(text.contains("SILENT▼") && text.contains("90×"), "{text}");

        press(&mut state, "s");
        assert_eq!(
            analytics_topics(&state),
            [
                "line/regular",
                "line/big",
                "line/wobbly",
                "line/gone",
                "line/spike",
                "other/x"
            ],
            "topics without a usual interval rank last"
        );
    }

    #[test]
    fn analytics_jitter_ranks_irregular_topics_and_skips_short_histories() {
        let (mut state, _directory) = timed_analytics_state();
        press(&mut state, "A4");
        assert_eq!(
            analytics_topics(&state),
            ["line/wobbly", "line/gone", "line/regular"],
            "topics with fewer than five messages are not judged"
        );
        assert!(screen(&mut state, 120, 20).contains("49%"));
        press(&mut state, "s");
        assert_eq!(
            analytics_topics(&state),
            ["line/gone", "line/wobbly", "line/regular"],
            "longest interval first"
        );
        assert!(screen(&mut state, 120, 20).contains("MAX▼"));
    }

    #[test]
    fn analytics_largest_compares_the_latest_payload_and_the_peak_in_the_history() {
        let (mut state, _directory) = timed_analytics_state();
        press(&mut state, "A5");
        assert_eq!(analytics_topics(&state)[..2], ["line/spike", "line/big"]);
        assert_eq!(state.analytics.rows[1].size, 20);
        assert_eq!(state.analytics.rows[1].peak, 5000);
        press(&mut state, "s");
        assert_eq!(analytics_topics(&state)[..2], ["line/big", "line/spike"]);
        assert!(screen(&mut state, 120, 20).contains("PEAK▼"));
    }

    #[test]
    fn analytics_retained_lists_topics_whose_latest_message_was_retained() {
        let (mut state, _directory) = timed_analytics_state();
        press(&mut state, "A6");
        assert_eq!(analytics_topics(&state), ["line/big"]);
        assert!(screen(&mut state, 120, 20).contains("yyyyyyyy"));
        state.apply_broker_event(message_at("line/big", b"live", false, state.now));
        state.tick(state.now + Duration::from_secs(2));
        screen(&mut state, 120, 20);
        assert!(analytics_topics(&state).is_empty());
    }

    #[test]
    fn analytics_branches_sum_up_groups_and_change_depth() {
        let (mut state, _directory) = timed_analytics_state();
        press(&mut state, "A7");
        assert_eq!(analytics_topics(&state), ["line", "other"]);
        assert_eq!(state.analytics.rows[0].topics, 5);
        assert_eq!(state.analytics.rows[0].count, 11 + 7 + 5 + 3 + 1);
        assert!(screen(&mut state, 120, 20).contains("first 1 topic level"));

        press(&mut state, "d");
        assert_eq!(state.analytics.depth, 2);
        assert_eq!(state.analytics.rows.len(), 6);
        assert_eq!(state.analytics.rows[0].topic, "line/regular");
        assert!(screen(&mut state, 120, 20).contains("first 2 topic levels"));

        press(&mut state, "ddddd");
        assert_eq!(state.analytics.depth, 1);
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.view, View::Main);
        assert_eq!(state.main.selected.as_deref(), Some("line"));
    }

    #[test]
    fn a_adds_brokers_only_in_the_broker_manager_and_the_bar_offers_analytics() {
        let (mut state, _directory) = state_with(&[("a/b", "1")]);
        press(&mut state, "a");
        assert!(state.editor.is_none(), "a does nothing in the explorer");
        assert!(screen(&mut state, 120, 20).contains("[A] Analytics"));
        press(&mut state, "ba");
        assert!(state.editor.is_some());
    }

    #[test]
    fn help_has_tabs_that_scroll_and_show_changelog_and_about() {
        let (mut state, _directory) = state_with(&[]);
        press(&mut state, "?");
        let keys = screen(&mut state, 100, 40);
        assert!(keys.contains("[1 Keys]") && keys.contains("Topic explorer"));

        press(&mut state, "3");
        assert!(screen(&mut state, 100, 40).contains("Where a message comes from"));
        press(&mut state, "4");
        let changelog = screen(&mut state, 100, 40);
        assert!(changelog.contains("[4 Changelog]") && changelog.contains("Added"));
        press(&mut state, "5");
        let about = screen(&mut state, 100, 40);
        assert!(about.contains(env!("CARGO_PKG_VERSION")) && about.contains("MIT"));
        assert!(about.contains("ui-state.toml"));

        handle_key(&mut state, KeyCode::BackTab);
        assert_eq!(state.help.tab, help::HelpTab::Changelog);
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.help.tab, help::HelpTab::Keys, "tabs wrap around");

        let first = screen(&mut state, 100, 14);
        assert!(first.contains("Global") && !first.contains("History"));
        press(&mut state, "G");
        let last = screen(&mut state, 100, 14);
        assert!(last.contains("Filter the messages") && !last.contains("Global"));
        assert!(last.contains("100%"));
        press(&mut state, "j");
        screen(&mut state, 100, 14);
        assert_eq!(
            state.help.scroll, state.help.max_scroll,
            "no scrolling past the end"
        );
        press(&mut state, "g");
        assert!(screen(&mut state, 100, 14).contains("Global"));

        press(&mut state, "q");
        assert!(!state.help_open);
    }

    #[test]
    fn the_hidden_snake_tab_is_played_with_the_arrow_keys() {
        use mate::snake::Phase;
        let (mut state, _directory) = state_with(&[]);
        press(&mut state, "?");
        assert!(
            !screen(&mut state, 100, 40).contains("Snake"),
            "hidden until asked for"
        );
        press(&mut state, "6");
        let text = screen(&mut state, 100, 40);
        assert!(
            text.contains("[6 Snake]") && text.contains("Score 0   Best 0"),
            "{text}"
        );
        assert!(text.contains("Press an arrow key or Enter to start"));
        assert!(text.contains("[\u{2190}\u{2191}\u{2193}\u{2192}] Steer"));

        let head = state.help.snake.as_ref().unwrap().head();
        state.tick(state.now + Duration::from_secs(1));
        assert_eq!(
            state.help.snake.as_ref().unwrap().head(),
            head,
            "waits for the start"
        );

        handle_key(&mut state, KeyCode::Up);
        assert_eq!(state.help.snake.as_ref().unwrap().phase(), Phase::Running);
        state.tick(state.now);
        state.tick(state.now + Duration::from_millis(200));
        assert_eq!(
            state.help.snake.as_ref().unwrap().head(),
            (head.0, head.1 - 1)
        );
        assert!(state.take_redraw(), "a step asks for a redraw");

        handle_key(&mut state, KeyCode::Left);
        state.tick(state.now + Duration::from_millis(200));
        assert_eq!(
            state.help.snake.as_ref().unwrap().head(),
            (head.0 - 1, head.1 - 1)
        );
        assert!(state.help.scroll == 0, "arrows steer, they do not scroll");

        handle_key(&mut state, KeyCode::Enter);
        let paused = state.help.snake.as_ref().unwrap().head();
        assert!(screen(&mut state, 100, 40).contains("Paused"));
        state.tick(state.now + Duration::from_secs(1));
        assert_eq!(state.help.snake.as_ref().unwrap().head(), paused);

        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.help.tab, help::HelpTab::Keys, "Tab leaves the game");
        assert_eq!(state.help.snake.as_ref().unwrap().phase(), Phase::Paused);
        state.tick(state.now + Duration::from_secs(1));
        assert_eq!(state.help.snake.as_ref().unwrap().head(), paused);

        press(&mut state, "6");
        handle_key(&mut state, KeyCode::Down);
        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.help_open);
        assert!(
            !state.help.snake.as_ref().unwrap().is_running(),
            "closing the help pauses"
        );
        let before = state.help.snake.as_ref().unwrap().head();
        state.tick(state.now + Duration::from_secs(5));
        assert_eq!(state.help.snake.as_ref().unwrap().head(), before);
    }

    #[test]
    fn the_snake_field_needs_room_and_crashing_ends_the_round() {
        let (mut state, _directory) = state_with(&[]);
        press(&mut state, "?");
        press(&mut state, "6");
        assert!(screen(&mut state, 100, 14).contains("Snake needs a larger window"));
        handle_key(&mut state, KeyCode::Up);
        for _ in 0..30 {
            state.tick(state.now + Duration::from_millis(200));
        }
        let text = screen(&mut state, 100, 40);
        assert!(text.contains("Crashed. Enter plays again"), "{text}");
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.help.snake.as_ref().unwrap().is_running());
    }

    #[test]
    fn help_opens_on_the_section_of_the_current_view() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A?");
        assert!(state.help_open && state.help.tab == help::HelpTab::Views);
        let text = screen(&mut state, 100, 20);
        let first_line = text.lines().nth(4).unwrap();
        assert!(first_line.contains("Analytics"), "{text}");
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.view, View::Analytics, "closing help keeps the view");

        press(&mut state, "A");
        press(&mut state, "b?");
        assert!(
            screen(&mut state, 100, 20)
                .lines()
                .nth(4)
                .unwrap()
                .contains("Broker manager")
        );
    }

    #[test]
    fn a_question_mark_in_a_search_is_text_not_help() {
        let (mut state, _directory) = state_with(&[("a/b", "1")]);
        press(&mut state, "/?");
        assert!(!state.help_open);
        assert_eq!(state.main.search.query, "?");
    }

    fn message_with_origin(topic: &str, payload: &str, origin: &str) -> BrokerEvent {
        BrokerEvent::Message(IncomingMessage {
            topic: topic.to_owned(),
            payload: payload.as_bytes().to_vec(),
            qos: 0,
            retain: false,
            dup: false,
            properties: Some(std::sync::Arc::new(mate::topics::MessageProperties {
                user_properties: vec![("origin".to_owned(), origin.to_owned())],
                ..Default::default()
            })),
            received: Instant::now(),
            wall: SystemTime::now(),
        })
    }

    fn shown_history(state: &UiState) -> Vec<String> {
        let topic = state.main.selected.as_deref().unwrap();
        main_view::history_messages(&state.topics, topic, &state.main.history_search.query)
            .iter()
            .map(|message| String::from_utf8_lossy(&message.payload).into_owned())
            .collect()
    }

    #[test]
    fn history_search_filters_by_payload_and_mqtt5_properties() {
        let (mut state, _directory) = state_with(&[("plc/t", "alpha"), ("plc/t", "BETA state")]);
        state.apply_broker_event(message_with_origin("plc/t", "gamma", "PLC-7"));
        press(&mut state, "h");
        state.main.reveal("plc/t");
        state.main.focus = Panel::History;
        screen(&mut state, 120, 30);
        assert_eq!(shown_history(&state), ["gamma", "BETA state", "alpha"]);

        press(&mut state, "/");
        assert!(state.main.history_search.active);
        press(&mut state, "beta");
        let text = screen(&mut state, 120, 30);
        assert_eq!(shown_history(&state), ["BETA state"]);
        assert!(text.contains("/beta"), "{text}");
        handle_key(&mut state, KeyCode::Enter);
        assert!(!state.main.history_search.active);
        assert_eq!(
            shown_history(&state),
            ["BETA state"],
            "Enter keeps the filter"
        );

        handle_key(&mut state, KeyCode::Esc);
        assert!(state.main.history_search.query.is_empty());
        assert_eq!(shown_history(&state).len(), 3);

        press(&mut state, "/origin=plc-7");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(shown_history(&state), ["gamma"]);
        handle_key(&mut state, KeyCode::Enter);
        let gamma = state.topics.latest_message("plc/t").unwrap().seq;
        assert_eq!(state.main.pinned, Some(gamma));

        press(&mut state, "/zzz");
        assert!(screen(&mut state, 120, 30).contains("No message matches"));
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(shown_history(&state).len(), 3);
    }

    #[test]
    fn typing_a_question_mark_or_q_into_the_history_search_does_not_open_help_or_quit() {
        let (mut state, _directory) = state_with(&[("a/b", "1")]);
        state.main.reveal("a/b");
        state.main.focus = Panel::History;
        press(&mut state, "/q?");
        assert!(!state.help_open);
        assert_eq!(state.main.history_search.query, "q?");
    }

    #[test]
    fn analytics_filter_narrows_every_list_and_keeps_typed_keys_out_of_the_commands() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A/cyc");
        assert!(state.analytics.filtering);
        assert_eq!(analytics_topics(&state), ["plc/cycle/busy"]);
        assert!(screen(&mut state, 120, 20).contains("/cyc"));
        press(&mut state, "q?");
        assert!(!state.help_open);
        assert_eq!(state.analytics.filter, "cycq?");
        handle_key(&mut state, KeyCode::Backspace);
        handle_key(&mut state, KeyCode::Backspace);
        handle_key(&mut state, KeyCode::Enter);
        assert!(!state.analytics.filtering);
        assert_eq!(analytics_topics(&state), ["plc/cycle/busy"]);

        press(&mut state, "7");
        assert_eq!(
            state.analytics.rows[0].topics, 1,
            "branch totals use the filter too"
        );

        handle_key(&mut state, KeyCode::Esc);
        assert!(state.analytics.filter.is_empty());
        assert_eq!(
            state.view,
            View::Analytics,
            "the first Esc only clears the filter"
        );
        assert_eq!(state.analytics.rows[0].topics, 5);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.view, View::Main);

        press(&mut state, "A/zzz");
        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.analytics.filtering && state.analytics.filter.is_empty());
        assert_eq!(state.analytics.rows[0].topics, 5);
    }

    #[test]
    fn analytics_pause_freezes_the_list_until_resumed() {
        let (mut state, _directory) = analytics_state();
        press(&mut state, "A");
        assert_eq!(state.analytics.rows[0].count, 50);
        press(&mut state, " ");
        assert!(state.analytics.paused);
        for cycle in 0..5 {
            state.apply_broker_event(message("plc/cycle/busy", &cycle.to_string()));
        }
        state.tick(state.now + Duration::from_secs(3));
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("PAUSED") && text.contains("[Space] Resume"),
            "{text}"
        );
        assert_eq!(state.analytics.rows[0].count, 50);

        press(&mut state, " ");
        assert!(!state.analytics.paused);
        assert_eq!(state.analytics.rows[0].count, 55);
        assert!(!screen(&mut state, 120, 20).contains("PAUSED"));
    }

    #[test]
    fn ctrl_o_loads_the_payload_from_a_text_file() {
        let (mut state, directory) = writable_state(&[("cmd", "1")]);
        let file = directory.path().join("payload.json");
        std::fs::write(&file, "{\"a\":1}\n").unwrap();
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        assert!(state.publish_editor.as_ref().unwrap().file_prompt.is_some());
        assert!(screen(&mut state, 140, 30).contains("Load file:"));

        handle_key(&mut state, KeyCode::Esc);
        assert!(state.publish_editor.is_some(), "Esc only closes the prompt");
        assert!(state.publish_editor.as_ref().unwrap().file_prompt.is_none());

        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        handle_paste(&mut state, &format!("{}\n", file.display()));
        handle_key(&mut state, KeyCode::Enter);
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.payload, "{\"a\":1}");
        assert!(editor.error.is_none() && editor.file_prompt.is_none());

        handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        press(&mut state, "/definitely/missing");
        handle_key(&mut state, KeyCode::Enter);
        let editor = state.publish_editor.as_ref().unwrap();
        assert!(editor.error.as_ref().unwrap().contains("Cannot read"));
        assert_eq!(
            editor.payload, "{\"a\":1}",
            "a failed load keeps the payload"
        );
    }

    fn stream_lines(state: &mut UiState) -> Vec<String> {
        screen(state, 120, 24).lines().map(str::to_owned).collect()
    }

    fn line_of(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not shown:\n{}", lines.join("\n")))
    }

    #[test]
    fn stream_lists_messages_and_connection_events_and_expands_details() {
        let (mut state, _directory) = state_with(&[]);
        state.apply_broker_event(message("plc/a", "1"));
        state.apply_broker_event(message_with_origin("plc/b", "{\"x\":1}", "plc-7"));
        press(&mut state, "S");
        assert_eq!(state.view, View::Stream);
        let lines = stream_lines(&mut state);
        assert!(lines[1].contains("STREAM") && lines[1].contains("FOLLOW"));
        assert!(
            line_of(&lines, "Connecting to Production") < line_of(&lines, "\u{25cf} Connected")
        );
        assert!(line_of(&lines, "\u{25cf} Connected") < line_of(&lines, "plc/a"));
        assert!(line_of(&lines, "plc/a") < line_of(&lines, "plc/b"));
        assert!(
            lines.iter().any(|line| line.contains("{\"x\":1}")),
            "the preview shows the payload"
        );

        handle_key(&mut state, KeyCode::Enter);
        let text = stream_lines(&mut state).join("\n");
        assert!(
            text.contains("QoS 0") && text.contains("origin = plc-7"),
            "{text}"
        );
        assert!(
            text.contains("\"x\": 1"),
            "the payload is formatted:\n{text}"
        );
        handle_key(&mut state, KeyCode::Enter);
        assert!(
            !stream_lines(&mut state)
                .join("\n")
                .contains("origin = plc-7")
        );

        press(&mut state, "?");
        assert!(state.help_open && state.help.jump == Some("Stream"));
        handle_key(&mut state, KeyCode::Esc);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.view, View::Main);
    }

    #[test]
    fn stream_filters_by_topic_payload_and_retain_flag_and_hides_events_then() {
        let (mut state, _directory) = state_with(&[]);
        state.apply_broker_event(message("plc/a", "one"));
        state.apply_broker_event(message("plc/b", "two"));
        state.apply_broker_event(message_with("plc/c", b"three", true));
        press(&mut state, "S/plc/b");
        assert!(
            stream_lines(&mut state)
                .iter()
                .any(|line| line.contains("topic /plc/b"))
        );
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.stream_view.shown().len(), 1);
        let text = stream_lines(&mut state).join("\n");
        assert!(
            text.contains("plc/b")
                && !text.contains("plc/a")
                && !text.contains("\u{25cf} Connected")
        );

        handle_key(&mut state, KeyCode::Esc);
        assert!(!state.stream_view.filters_active());
        assert_eq!(
            state.view,
            View::Stream,
            "the first Esc only clears the filters"
        );
        assert_eq!(
            state.stream_view.shown().len(),
            5,
            "two events and three messages"
        );

        press(&mut state, "fTHREE");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(
            state.stream_view.shown().len(),
            1,
            "text search ignores ASCII case"
        );
        handle_key(&mut state, KeyCode::Esc);
        press(&mut state, "R");
        assert_eq!(state.stream_view.shown().len(), 1);
        press(&mut state, "R");
        press(&mut state, "e");
        assert_eq!(state.stream_view.shown().len(), 3, "events can be hidden");
    }

    #[test]
    fn stream_clear_empties_the_list_but_keeps_the_filters_and_the_topics() {
        let (mut state, _directory) = state_with(&[]);
        state.apply_broker_event(message("plc/a", "one"));
        state.apply_broker_event(message("plc/b", "two"));
        press(&mut state, "S/plc");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.stream_view.shown().len(), 2);

        press(&mut state, "C");
        assert_eq!(state.stream_view.shown().len(), 0);
        assert!(state.stream.is_empty());
        assert_eq!(state.stream_view.topic_filter, "plc", "the filter survives");
        assert_eq!(
            state.topics.topic_count(),
            2,
            "the topic data is not touched"
        );
        assert!(state.notice.as_ref().unwrap().text.contains("cleared"));

        state.apply_broker_event(message("plc/c", "three"));
        state.apply_broker_event(message("other", "x"));
        assert_eq!(
            state.stream_view.shown().len(),
            0,
            "refreshed on the next draw"
        );
        let text = stream_lines(&mut state).join("\n");
        assert!(
            text.contains("plc/c") && !text.contains("plc/a") && !text.contains("other"),
            "{text}"
        );
    }

    #[test]
    fn stream_pause_holds_new_entries_back_until_resumed() {
        let (mut state, _directory) = state_with(&[("plc/a", "1")]);
        press(&mut state, "S ");
        assert!(state.stream_view.paused);
        let before = state.stream_view.shown().len();
        state.apply_broker_event(message("plc/b", "2"));
        let text = stream_lines(&mut state).join("\n");
        assert!(
            text.contains("PAUSED") && text.contains("+1 waiting"),
            "{text}"
        );
        assert_eq!(state.stream_view.shown().len(), before);
        assert!(!text.contains("plc/b"));
        press(&mut state, " ");
        assert!(stream_lines(&mut state).join("\n").contains("plc/b"));
    }

    #[test]
    fn stream_navigation_switches_follow_off_and_on_and_entries_open_in_the_explorer() {
        let (mut state, _directory) = state_with(&[("plc/a", "1"), ("plc/b/c", "{\"k\":2}")]);
        press(&mut state, "S");
        assert!(state.stream_view.follow);
        press(&mut state, "k");
        assert!(!state.stream_view.follow);
        press(&mut state, "G");
        assert!(state.stream_view.follow);

        press(&mut state, "c");
        assert_eq!(state.take_clipboard_request().unwrap().text, "{\"k\":2}");
        press(&mut state, "t");
        assert_eq!(state.take_clipboard_request().unwrap().text, "plc/b/c");
        press(&mut state, "s");
        assert_eq!(state.view, View::Main);
        assert_eq!(state.main.selected.as_deref(), Some("plc/b/c"));

        press(&mut state, "S");
        press(&mut state, "gs");
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Select a message"),
            "events cannot be opened"
        );
    }

    fn ctrl(state: &mut UiState, character: char) -> UiAction {
        handle_key_event(
            state,
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
        )
    }

    #[test]
    fn the_publish_panel_docks_below_the_panes_takes_typing_and_keeps_its_draft() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "P");
        assert!(state.main.layout.show_publish);
        assert_eq!(state.main.focus, Panel::Publish);
        assert!(state.publish_editor.as_ref().unwrap().docked);
        let text = screen(&mut state, 140, 40);
        for expected in ["TOPICS", "PAYLOAD INSPECTOR", "PUBLISH \u{b7} Production"] {
            assert!(text.contains(expected), "{expected}:\n{text}");
        }
        assert!(text.contains("[Esc] Leave panel"));

        handle_key(&mut state, KeyCode::Tab);
        assert!(
            screen(&mut state, 60, 40).contains("[Esc] Leave panel"),
            "the way out is shown first, also in a narrow bar"
        );
        press(&mut state, "phqm hi");
        assert_eq!(state.publish_editor.as_ref().unwrap().payload, "phqm hi");
        assert!(
            !state.main.layout.show_history,
            "typed letters are not shortcuts"
        );

        handle_key(&mut state, KeyCode::Esc);
        assert_ne!(state.main.focus, Panel::Publish);
        assert!(state.publish_editor.is_some(), "Esc only leaves the panel");
        press(&mut state, "h");
        assert!(
            state.main.layout.show_history,
            "shortcuts work outside the panel"
        );

        press(&mut state, "P");
        assert!(!state.main.layout.show_publish && state.publish_editor.is_none());
        assert!(state.publish_draft.is_some());
        press(&mut state, "P");
        assert_eq!(state.publish_editor.as_ref().unwrap().payload, "phqm hi");
        assert_eq!(state.main.focus, Panel::Publish);

        let UiAction::Publish(_, request) = ctrl(&mut state, 's') else {
            panic!("Ctrl+S sends from the panel");
        };
        assert_eq!(
            (request.topic.as_str(), request.payload.as_slice()),
            ("cmd", &b"phqm hi"[..])
        );
        let editor = state
            .publish_editor
            .as_ref()
            .expect("the draft stays after sending");
        assert!(editor.docked && editor.payload == "phqm hi");
    }

    #[test]
    fn p_opens_the_dialog_unless_the_panel_is_shown_and_the_review_pops_up_over_the_panel() {
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        assert!(
            !state.publish_editor.as_ref().unwrap().docked,
            "the old dialog stays"
        );
        handle_key(&mut state, KeyCode::Esc);
        assert!(state.publish_editor.is_none());

        press(&mut state, "P");
        handle_key(&mut state, KeyCode::Esc);
        press(&mut state, "p");
        assert_eq!(
            state.main.focus,
            Panel::Publish,
            "p focuses the shown panel"
        );
        assert!(state.publish_editor.as_ref().unwrap().docked);

        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Enter);
        }
        assert_eq!(
            state.publish_editor.as_ref().unwrap().stage,
            publish::PublishStage::Review
        );
        assert!(screen(&mut state, 140, 40).contains("REVIEW PUBLISH"));
        assert!(matches!(
            handle_key(&mut state, KeyCode::Char('y')),
            UiAction::Publish(..)
        ));
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(
            editor.stage,
            publish::PublishStage::Edit,
            "back to editing after sending"
        );
        assert!(!screen(&mut state, 140, 40).contains("REVIEW PUBLISH"));
    }

    #[test]
    fn the_publish_panel_is_refused_without_permission_and_joins_the_focus_cycle() {
        let (mut state, _directory) = state_with(&[("cmd", "1")]);
        press(&mut state, "P");
        assert!(!state.main.layout.show_publish);
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Publishing is off")
        );

        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "P");
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.main.focus, Panel::Topics);
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, Panel::Publish);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.main.focus, Panel::Topics);
    }

    #[test]
    fn the_certificate_path_has_a_free_cursor() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(AppConfig::default(), store, None);
        state.view = View::Certificates;
        open_certificate_editor(&mut state, CertificateEditMode::Reference);
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "/etc/ca.pem");
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Left);
        }
        press(&mut state, "-new");
        assert_eq!(
            state.certificate_editor.as_ref().unwrap().path,
            "/etc/ca-new.pem"
        );

        handle_key(&mut state, KeyCode::Home);
        handle_key(&mut state, KeyCode::Delete);
        assert_eq!(
            state.certificate_editor.as_ref().unwrap().path,
            "etc/ca-new.pem"
        );
        handle_key(&mut state, KeyCode::End);
        handle_key(&mut state, KeyCode::Backspace);
        handle_paste(&mut state, " 7\n");
        assert_eq!(
            state.certificate_editor.as_ref().unwrap().path,
            "etc/ca-new.pe7"
        );

        handle_key(&mut state, KeyCode::Home);
        handle_key(&mut state, KeyCode::Left);
        handle_key(&mut state, KeyCode::Backspace);
        assert_eq!(
            state.certificate_editor.as_ref().unwrap().cursor,
            0,
            "no editing before the start"
        );
        for _ in 0..40 {
            handle_key(&mut state, KeyCode::Right);
        }
        assert_eq!(state.certificate_editor.as_ref().unwrap().cursor, 14);
        assert!(screen(&mut state, 100, 20).contains("Path: > etc/ca-new.pe7"));
    }

    #[test]
    fn exclusions_are_added_toggled_and_removed_in_the_subscription_manager_and_applied_live() {
        let (store, _directory) = temp_store();
        let broker = profile(&store, "Lab");
        store.add_subscription(&broker.id, "#", 0).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        state.open_subscriptions(broker.id.clone());

        press(&mut state, "x");
        assert!(
            state
                .subscriptions
                .as_ref()
                .unwrap()
                .editor
                .as_ref()
                .unwrap()
                .exclude
        );
        let text = screen(&mut state, 120, 24);
        assert!(
            text.contains("ADD EXCLUSION") && !text.contains("QoS (0/1/2)"),
            "{text}"
        );
        assert!(text.contains("[Enter] Save"));
        press(&mut state, "a/#/b");
        assert!(matches!(
            handle_key(&mut state, KeyCode::Enter),
            UiAction::None
        ));
        let error = state
            .subscriptions
            .as_ref()
            .unwrap()
            .editor
            .as_ref()
            .unwrap()
            .error
            .clone();
        assert!(
            error.unwrap().contains("not a valid"),
            "an invalid filter is refused"
        );
        for _ in 0..5 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "+/hmi/#");
        assert!(
            matches!(handle_key(&mut state, KeyCode::Enter), UiAction::None),
            "no live change while not connected"
        );
        let saved = store.get(&broker.id).unwrap();
        assert!(saved.subscriptions[1].exclude && saved.subscriptions[1].topic == "+/hmi/#");
        let text = screen(&mut state, 120, 24);
        assert!(
            text.contains("+/hmi/#") && text.contains("exclude"),
            "{text}"
        );
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("already received")
        );

        connected(&mut state, &broker);
        assert_eq!(state.subscriptions.as_ref().unwrap().selection, 1);
        let live = |action: UiAction| match action {
            UiAction::Subscription(_, change) => change,
            _ => panic!("a live change was expected"),
        };
        press(&mut state, "2");
        assert_eq!(
            store.get(&broker.id).unwrap().subscriptions[1].qos,
            0,
            "QoS does not apply to exclusions"
        );
        assert_eq!(
            live(handle_key(&mut state, KeyCode::Char(' '))),
            SubscriptionChange::Unexclude {
                topic: "+/hmi/#".to_owned()
            }
        );
        assert!(!store.get(&broker.id).unwrap().subscriptions[1].enabled);
        assert_eq!(
            live(handle_key(&mut state, KeyCode::Char(' '))),
            SubscriptionChange::Exclude {
                topic: "+/hmi/#".to_owned()
            }
        );
        assert_eq!(
            live(handle_key(&mut state, KeyCode::Char('d'))),
            SubscriptionChange::Unexclude {
                topic: "+/hmi/#".to_owned()
            }
        );
        assert_eq!(store.get(&broker.id).unwrap().subscriptions.len(), 1);
    }

    #[test]
    fn stream_entries_open_and_close_with_enter_plus_and_minus() {
        let (mut state, _directory) = state_with(&[("plc/a", "{\"k\":1}")]);
        press(&mut state, "S");
        let details = |state: &mut UiState| stream_lines(state).join("\n").contains("QoS 0");
        assert!(!details(&mut state));
        handle_key(&mut state, KeyCode::Enter);
        assert!(details(&mut state));
        handle_key(&mut state, KeyCode::Enter);
        assert!(!details(&mut state));

        press(&mut state, "+");
        assert!(details(&mut state));
        press(&mut state, "+");
        assert!(details(&mut state), "+ only opens");
        press(&mut state, "-");
        assert!(!details(&mut state));
        press(&mut state, "-");
        assert!(!details(&mut state), "- only closes");

        press(&mut state, "k");
        assert!(state.stream_view.selected_id().is_some());
        press(&mut state, "+-");
        assert!(
            state.stream_view.expanded.is_empty(),
            "connection events have no details"
        );
    }

    #[test]
    fn the_broker_manager_shows_and_hides_system_topics_live() {
        let (store, _directory) = temp_store();
        let lab = profile(&store, "Lab");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        press(&mut state, "b");
        assert!(screen(&mut state, 140, 30).contains("hidden (y to show)"));

        assert!(
            matches!(handle_key(&mut state, KeyCode::Char('y')), UiAction::None),
            "no live change while disconnected"
        );
        assert!(store.get(&lab.id).unwrap().show_system_topics);
        assert!(screen(&mut state, 140, 30).contains("shown, $SYS/# (y to hide)"));
        handle_key(&mut state, KeyCode::Char('y'));
        assert!(!store.get(&lab.id).unwrap().show_system_topics);

        connected(&mut state, &lab);
        let live = |action: UiAction| match action {
            UiAction::Subscription(_, change) => change,
            _ => panic!("a live change was expected"),
        };
        assert_eq!(
            live(handle_key(&mut state, KeyCode::Char('y'))),
            SubscriptionChange::Subscribe {
                topic: "$SYS/#".to_owned(),
                qos: 0
            }
        );
        assert_eq!(
            live(handle_key(&mut state, KeyCode::Char('y'))),
            SubscriptionChange::Unsubscribe {
                topic: "$SYS/#".to_owned()
            }
        );

        store.add_subscription(&lab.id, "$SYS/#", 0).unwrap();
        reload_profiles(&mut state);
        handle_key(&mut state, KeyCode::Char('y'));
        assert!(
            matches!(handle_key(&mut state, KeyCode::Char('y')), UiAction::None),
            "an explicit $SYS subscription stays when the option is switched off"
        );
    }

    #[test]
    fn the_terminal_title_names_the_broker_and_its_state_without_control_characters() {
        let (store, _directory) = temp_store();
        let lab = profile(&store, "Lab");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        assert_eq!(state.window_title(), "MATE");

        state.selected_broker = Some(lab.id.clone());
        assert_eq!(state.window_title(), "Lab \u{b7} MATE");
        state.begin_connection(&lab);
        assert_eq!(state.window_title(), "Lab \u{2013} Connecting \u{b7} MATE");
        state.apply_broker_event(BrokerEvent::State {
            state: ConnectionState::Connected,
            detail: None,
        });
        assert_eq!(state.window_title(), "Lab \u{2013} Connected \u{b7} MATE");
        state.apply_broker_event(BrokerEvent::Failed("no".to_owned()));
        assert_eq!(state.window_title(), "Lab \u{2013} Error \u{b7} MATE");
        state.mark_disconnected();
        assert_eq!(
            state.window_title(),
            "Lab \u{2013} Disconnected \u{b7} MATE"
        );

        let hostile = store
            .create(BrokerConfig {
                name: "Evil\x1b]0;pwned\x07 Name".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        state.begin_connection(&hostile);
        let title = state.window_title();
        assert!(!title.chars().any(char::is_control), "{title:?}");
        assert!(title.starts_with("Evil]0;pwned Name"));

        let mut long = lab.clone();
        long.name = "x".repeat(200);
        state.begin_connection(&long);
        assert_eq!(state.window_title().chars().count(), 80);
    }

    #[test]
    fn startup_connects_with_the_flag_or_when_the_chosen_profile_asks_for_it() {
        let (store, _directory) = temp_store();
        let alpha = profile(&store, "Alpha");
        let beta = store
            .create(BrokerConfig {
                name: "Beta".to_owned(),
                host: "beta.example.test".to_owned(),
                auto_connect: true,
                ..BrokerConfig::default()
            })
            .unwrap();
        let connects = |action: UiAction, id: &str| matches!(action, UiAction::Connect(profile) if profile.id == id);

        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        assert!(
            matches!(state.startup_connect(false), UiAction::None),
            "nothing chosen"
        );
        assert!(matches!(state.startup_connect(true), UiAction::None));
        assert_eq!(
            state.view,
            View::Brokers,
            "--connect without a clear choice opens the list"
        );

        let mut state = UiState::new(store.load().unwrap(), store.clone(), Some(beta.id.clone()));
        assert!(
            connects(state.startup_connect(false), &beta.id),
            "auto_connect profile"
        );
        let mut state = UiState::new(store.load().unwrap(), store.clone(), Some(alpha.id.clone()));
        assert!(
            matches!(state.startup_connect(false), UiAction::None),
            "no auto_connect"
        );
        assert!(
            connects(state.startup_connect(true), &alpha.id),
            "--connect"
        );

        store.set_default_broker(Some(&beta.id)).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        assert!(
            connects(state.startup_connect(false), &beta.id),
            "the default broker counts"
        );

        store.remove(&beta.id).unwrap();
        let mut state = UiState::new(store.load().unwrap(), store, None);
        assert!(
            connects(state.startup_connect(true), &alpha.id),
            "the only broker"
        );
    }

    #[test]
    fn two_instances_do_not_overwrite_each_others_saved_settings() {
        let (store, _directory) = temp_store();
        let lab = profile(&store, "Lab");
        let mut first = UiState::new(store.load().unwrap(), store.clone(), None);
        let mut second = UiState::new(store.load().unwrap(), store.clone(), None);

        connected(&mut first, &lab);
        first.apply_broker_event(message("plant/status", "RUNNING"));
        press(&mut first, "*");
        assert!(
            first.main.favorites.contains("plant"),
            "the first instance marked a favorite"
        );

        // The second instance loaded before that and only changes the layout.
        press(&mut second, "h");
        let saved = UiSettings::load(store.root()).unwrap();
        assert!(
            saved.layout.as_ref().unwrap().show_history,
            "the layout of the second instance"
        );
        assert!(
            saved.favorites[&lab.id].contains("plant"),
            "the favorite of the first instance survived"
        );
    }

    /// Foreground color and cursor flag of every drawn cell with this symbol.
    fn styled_cells(
        state: &mut UiState,
        symbol: &str,
    ) -> Vec<(Option<ratatui::style::Color>, bool)> {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut found = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == symbol {
                    let style = cell.style();
                    let reversed = style
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED);
                    found.push((style.fg, reversed));
                }
            }
        }
        found
    }

    #[test]
    fn json_payloads_are_colored_in_the_publish_editor_and_the_review() {
        use ratatui::style::Color;
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "{\"kzk\": 42, \"n\": \"wqw\", \"ok\": true}");
        let colored = |state: &mut UiState, symbol: &str| -> Vec<Option<Color>> {
            styled_cells(state, symbol)
                .into_iter()
                .map(|(color, _)| color)
                .collect()
        };
        assert!(
            colored(&mut state, "z").contains(&Some(Color::Cyan)),
            "keys"
        );
        assert!(
            colored(&mut state, "q").contains(&Some(Color::Green)),
            "string values"
        );
        assert!(
            colored(&mut state, "4").contains(&Some(Color::Yellow)),
            "numbers"
        );
        assert!(colored(&mut state, "k").contains(&Some(Color::Cyan)));

        // The cursor sits behind the last character, shown as a reversed blank.
        let blanks = styled_cells(&mut state, " ");
        assert!(
            blanks.iter().any(|(_, reversed)| *reversed),
            "the cursor is visible"
        );
        handle_key(&mut state, KeyCode::Home);
        let at_start = styled_cells(&mut state, "{");
        assert!(
            at_start.iter().any(|(_, reversed)| *reversed),
            "the cursor marks the first character"
        );

        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Enter);
        }
        assert!(screen(&mut state, 140, 40).contains("REVIEW PUBLISH"));
        assert!(
            colored(&mut state, "z").contains(&Some(Color::Cyan)),
            "the review colors JSON too"
        );
    }

    #[test]
    fn text_payloads_stay_plain_in_the_publish_editor() {
        use ratatui::style::Color;
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "plain zq 42 text");
        let cells = styled_cells(&mut state, "z");
        assert!(
            cells
                .iter()
                .all(|(color, _)| *color != Some(Color::Cyan) && *color != Some(Color::Green))
        );
        let digits = styled_cells(&mut state, "4");
        assert!(
            digits
                .iter()
                .all(|(color, _)| *color != Some(Color::Yellow))
        );
    }

    /// "Alpha" and "Beta", both allowing publishing, connected together and showing flat topics.
    fn two_brokers() -> (UiState, BrokerConfig, BrokerConfig, tempfile::TempDir) {
        let (store, directory) = temp_store();
        let mut alpha = profile(&store, "Alpha");
        alpha.allow_publish = true;
        let mut beta = profile(&store, "Beta");
        beta.allow_publish = true;
        let mut state = UiState::new(store.load().unwrap(), store, None);
        state.begin_connections(&[alpha.clone(), beta.clone()]);
        for broker in [&alpha, &beta] {
            state.apply_event_from(
                &broker.id,
                BrokerEvent::State {
                    state: ConnectionState::Connected,
                    detail: None,
                },
            );
        }
        state.apply_event_from(&alpha.id, message("alpha_only", "1"));
        state.apply_event_from(&beta.id, message("beta_only", "2"));
        state.apply_event_from(&alpha.id, message("both", "3"));
        state.apply_event_from(&beta.id, message("both", "4"));
        (state, alpha, beta, directory)
    }

    #[test]
    fn topics_of_several_brokers_share_one_tree_and_name_their_brokers() {
        let (mut state, alpha, beta, _directory) = two_brokers();
        let names = |topic: &str| -> Vec<&str> {
            state
                .topics
                .topic_brokers(topic)
                .iter()
                .map(|broker| &**broker)
                .collect()
        };
        assert_eq!(names("alpha_only"), [alpha.id.as_str()]);
        assert_eq!(names("beta_only"), [beta.id.as_str()]);
        assert_eq!(names("both"), [alpha.id.as_str(), beta.id.as_str()]);
        assert_eq!(state.topics.topic_count(), 3);

        let text = screen(&mut state, 120, 24);
        assert!(text.contains("Alpha") && text.contains("Beta"), "{text}");
        let row = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no row with {needle}\n{text}"))
                .to_owned()
        };
        assert!(row("alpha_only").contains("1\u{b7} alpha_only"));
        assert!(row("beta_only").contains("\u{b7}2 beta_only"));
        assert!(row("both").contains("12 both"));
    }

    #[test]
    fn a_single_broker_shows_no_markers() {
        let (mut state, _directory) = state_with(&[("alpha_only", "1")]);
        let text = screen(&mut state, 120, 24);
        let row = text
            .lines()
            .find(|line| line.contains("alpha_only"))
            .unwrap();
        assert!(!row.contains("\u{b7}"), "{row}");
        assert!(
            state
                .broker_badge(&state.connections[0].broker_id)
                .is_none()
        );
    }

    #[test]
    fn the_stream_marks_the_broker_of_every_message() {
        let (mut state, _alpha, _beta, _directory) = two_brokers();
        press(&mut state, "S");
        let text = screen(&mut state, 140, 24);
        let row = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no row with {needle}\n{text}"))
                .to_owned()
        };
        assert!(row("alpha_only").contains("1 alpha_only"), "{text}");
        assert!(row("beta_only").contains("2 beta_only"), "{text}");
        assert!(
            text.contains("Alpha: Connected") || text.contains("Connecting to Alpha"),
            "events name their broker\n{text}"
        );
    }

    #[test]
    fn events_of_unknown_brokers_are_ignored_and_state_is_kept_per_broker() {
        let (mut state, alpha, beta, _directory) = two_brokers();
        state.apply_event_from("ghost", message("ghost_topic", "x"));
        assert_eq!(state.topics.topic_count(), 3);

        state.apply_event_from(
            &beta.id,
            BrokerEvent::State {
                state: ConnectionState::Reconnecting,
                detail: Some("gone".to_owned()),
            },
        );
        assert_eq!(
            state.connection_of(&alpha.id).unwrap().state,
            ConnectionState::Connected
        );
        assert_eq!(
            state.connection_of(&beta.id).unwrap().state,
            ConnectionState::Reconnecting
        );
        assert!(state.any_connected());
        let title = state.window_title();
        assert!(
            title.contains("Alpha, Beta") && title.contains("1/2 connected"),
            "{title}"
        );
    }

    #[test]
    fn starting_the_same_set_of_brokers_keeps_the_topics_another_set_drops_them() {
        let (mut state, alpha, beta, _directory) = two_brokers();
        state.begin_connections(&[beta.clone(), alpha.clone()]);
        assert_eq!(
            state.topics.topic_count(),
            3,
            "same brokers, cached topics stay"
        );
        state.begin_connections(std::slice::from_ref(&alpha));
        assert_eq!(state.topics.topic_count(), 0);
        assert!(state.broker_badge(&alpha.id).is_none());
    }

    #[test]
    fn messages_are_published_to_the_broker_of_the_selected_topic_and_ctrl_b_switches() {
        let (mut state, alpha, beta, _directory) = two_brokers();
        press(&mut state, "j");
        assert_eq!(state.main.selected.as_deref(), Some("beta_only"));
        assert_eq!(
            state.default_publish_broker().as_deref(),
            Some(beta.id.as_str())
        );

        press(&mut state, "p");
        assert_eq!(state.publish_editor.as_ref().unwrap().broker_id, beta.id);
        let text = screen(&mut state, 120, 40);
        assert!(text.contains("Ctrl+B changes"), "{text}");

        ctrl(&mut state, 'b');
        assert_eq!(state.publish_editor.as_ref().unwrap().broker_id, alpha.id);
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "hello");
        match ctrl(&mut state, 's') {
            UiAction::Publish(broker, request) => {
                assert_eq!(broker, alpha.id);
                assert_eq!(request.payload, b"hello");
            }
            _ => panic!("expected a publish"),
        }
        state.report_publish(
            &alpha.id,
            &PublishRequest {
                topic: "beta_only".to_owned(),
                payload: b"hello".to_vec(),
                qos: 0,
                retain: false,
                user_properties: Vec::new(),
            },
            Ok(()),
        );
        assert!(state.notice.as_ref().unwrap().text.contains("on Alpha"));
        assert_eq!(state.publish_history_of(&alpha.id).len(), 1);
        assert!(state.publish_history_of(&beta.id).is_empty());
    }

    #[test]
    fn a_broker_that_does_not_allow_publishing_is_skipped_by_ctrl_b() {
        let (mut state, alpha, mut beta, _directory) = two_brokers();
        beta.allow_publish = false;
        state.begin_connections(&[alpha.clone(), beta.clone()]);
        for broker in [&alpha, &beta] {
            state.apply_event_from(
                &broker.id,
                BrokerEvent::State {
                    state: ConnectionState::Connected,
                    detail: None,
                },
            );
        }
        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.broker_id, alpha.id);
        ctrl(&mut state, 'b');
        assert_eq!(state.publish_editor.as_ref().unwrap().broker_id, alpha.id);
    }

    #[test]
    fn marking_brokers_in_the_manager_connects_all_of_them() {
        let (store, _directory) = temp_store();
        let alpha = profile(&store, "Alpha");
        let beta = profile(&store, "Beta");
        profile(&store, "Gamma");
        let mut state = UiState::new(store.load().unwrap(), store, None);

        press(&mut state, "b");
        press(&mut state, " jj ");
        assert_eq!(state.marked_brokers.len(), 2);
        let text = screen(&mut state, 120, 24);
        assert!(text.contains('\u{2713}'), "{text}");
        match handle_key(&mut state, KeyCode::Enter) {
            UiAction::ConnectMany(profiles) => {
                let ids: Vec<&str> = profiles.iter().map(|profile| profile.id.as_str()).collect();
                assert_eq!(ids, [alpha.id.as_str(), "gamma"]);
            }
            _ => panic!("expected several connections"),
        }

        // One mark connects only that broker; no mark connects the highlighted one.
        state.marked_brokers.clear();
        state.view = View::Brokers;
        state.broker_selection = 1;
        match handle_key(&mut state, KeyCode::Enter) {
            UiAction::Connect(profile) => assert_eq!(profile.id, beta.id),
            _ => panic!("expected one connection"),
        }
    }

    #[test]
    fn several_brokers_on_the_command_line_connect_together() {
        let (store, _directory) = temp_store();
        profile(&store, "Alpha");
        profile(&store, "Beta");
        let mut state = UiState::new(store.load().unwrap(), store, Some("Alpha".to_owned()));
        state.mark_brokers(&["Alpha".to_owned(), "beta".to_owned()]);
        assert!(matches!(
            state.startup_connect(true),
            UiAction::ConnectMany(profiles) if profiles.len() == 2
        ));
        assert!(matches!(state.startup_connect(false), UiAction::None));
    }

    #[test]
    fn the_logging_dialog_switches_the_files_on_and_remembers_the_choice() {
        let (store, directory) = temp_store();
        profile(&store, "Alpha");
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        assert!(state.log_directory().is_none());

        press(&mut state, "L");
        assert!(state.log_dialog.is_some());
        let text = screen(&mut state, 120, 30);
        assert!(
            text.contains("LOGGING") && text.contains("alpha_"),
            "{text}"
        );
        press(&mut state, " ");
        handle_key(&mut state, KeyCode::Tab);
        let wanted = directory.path().join("captures");
        press(&mut state, wanted.to_str().unwrap());
        handle_key(&mut state, KeyCode::Enter);

        assert!(state.log_dialog.is_none());
        assert_eq!(state.log_directory().as_deref(), Some(wanted.as_path()));
        assert!(wanted.is_dir(), "the directory is created right away");
        let saved = UiSettings::load(store.root()).unwrap();
        assert!(saved.logging.enabled);
        assert_eq!(saved.logging.directory, wanted.to_str().unwrap());

        // Switching it off again keeps the directory.
        press(&mut state, "L");
        press(&mut state, " ");
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.log_directory().is_none());
        assert_eq!(state.log.directory, wanted.to_str().unwrap());
    }

    #[test]
    fn an_unusable_log_directory_keeps_the_dialog_open_and_logging_off() {
        let (store, directory) = temp_store();
        let mut state = UiState::new(store.load().unwrap(), store, None);
        let file = directory.path().join("file");
        std::fs::write(&file, "x").unwrap();

        press(&mut state, "L");
        press(&mut state, " ");
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, file.join("below").to_str().unwrap());
        handle_key(&mut state, KeyCode::Enter);
        let dialog = state.log_dialog.as_ref().expect("still open");
        assert!(dialog.error.is_some());
        assert!(state.log_directory().is_none());

        handle_key(&mut state, KeyCode::Esc);
        assert!(state.log_dialog.is_none());
    }

    #[test]
    fn the_size_limit_is_edited_checked_and_remembered() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(store.load().unwrap(), store.clone(), None);
        assert_eq!(state.log_max_bytes(), 20 * 1024 * 1024);

        press(&mut state, "L");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        for _ in 0..4 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "x7a");
        assert_eq!(state.log_dialog.as_ref().unwrap().size, "7");
        let text = screen(&mut state, 120, 30);
        assert!(
            text.contains("7 MB") && text.contains("Max. size"),
            "{text}"
        );
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.log_dialog.is_none());
        assert_eq!(state.log_max_bytes(), 7 * 1024 * 1024);
        assert_eq!(
            UiSettings::load(store.root())
                .unwrap()
                .logging
                .max_megabytes,
            7
        );

        press(&mut state, "L");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Backspace);
        handle_key(&mut state, KeyCode::Enter);
        assert!(
            state.log_dialog.as_ref().unwrap().error.is_some(),
            "an empty limit is refused"
        );
        assert_eq!(state.log_max_bytes(), 7 * 1024 * 1024);
    }

    #[test]
    fn an_unusable_log_format_is_reported_and_the_default_is_used() {
        let (store, _directory) = temp_store();
        let mut config = store.load().unwrap();
        config.log_format.line = "{topic} {unknown}".to_owned();
        let state = UiState::new(config, store, None);
        let notice = state.notice.as_ref().expect("explained");
        assert!(
            notice.error && notice.text.contains("{unknown}"),
            "{}",
            notice.text
        );
        let line = state
            .log_format()
            .format_line(SystemTime::UNIX_EPOCH, "t", b"x", None);
        assert!(
            line.starts_with("1970-01-01 00:00:00,000 | T0   I: t []"),
            "{line}"
        );
    }

    const TEMPLATE: &str = r#"{"header":{"sequence":41,"name":"x"},"data":{}}"#;

    #[test]
    fn p_in_the_inspector_starts_from_the_shown_message_with_the_cursor_at_its_start() {
        let (mut state, _directory) = writable_state(&[]);
        state.apply_broker_event(message_with("cmd", TEMPLATE.as_bytes(), true));

        // The topics pane still starts with an empty form.
        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(
            (editor.topic.as_str(), editor.payload.as_str()),
            ("cmd", "")
        );
        handle_key(&mut state, KeyCode::Esc);

        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, layout::Panel::Inspector);
        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.topic, "cmd");
        assert_eq!(editor.payload, TEMPLATE);
        assert_eq!((editor.field, editor.cursor), (publish::FIELD_PAYLOAD, 0));
        assert!(editor.retain);

        // Typing lands at the start; Enter then reviews, and the sent payload is the edited one.
        press(&mut state, "  ");
        assert!(
            state
                .publish_editor
                .as_ref()
                .unwrap()
                .payload
                .starts_with("  {")
        );
        handle_key(&mut state, KeyCode::Esc);
        assert!(state.publish_editor.is_none());
    }

    #[test]
    fn p_in_the_history_uses_the_message_under_the_cursor() {
        let (mut state, _directory) = writable_state(&[("cmd", "old")]);
        state.apply_broker_event(message("cmd", "new"));
        press(&mut state, "h");
        handle_key(&mut state, KeyCode::Tab);
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, layout::Panel::History);
        press(&mut state, "j");
        let seq = state.history_cursor_seq().unwrap();
        let wanted = state
            .topics
            .find_message("cmd", seq)
            .unwrap()
            .payload
            .to_vec();

        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.payload.as_bytes(), wanted.as_slice());
        assert_eq!(editor.cursor, 0);
        assert_eq!(editor.field, publish::FIELD_PAYLOAD);
    }

    #[test]
    fn a_binary_payload_is_not_copied_and_the_panel_takes_the_template_too() {
        let (mut state, _directory) = writable_state(&[]);
        state.apply_broker_event(message_with("bin", &[0xff, 0x00], false));
        handle_key(&mut state, KeyCode::Tab);
        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert_eq!(editor.payload, "");
        assert!(state.notice.as_ref().unwrap().text.contains("binary"));
        handle_key(&mut state, KeyCode::Esc);

        // With the panel open, p on a message replaces the panel's draft.
        state.apply_broker_event(message("txt", TEMPLATE));
        press(&mut state, "P");
        press(&mut state, "draft");
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.main.focus, layout::Panel::Topics);
        press(&mut state, "j");
        handle_key(&mut state, KeyCode::Tab);
        assert_eq!(state.main.focus, layout::Panel::Inspector);
        press(&mut state, "p");
        let editor = state.publish_editor.as_ref().unwrap();
        assert!(editor.docked);
        assert_eq!(
            (editor.topic.as_str(), editor.payload.as_str()),
            ("txt", TEMPLATE)
        );
        assert_eq!(state.main.focus, layout::Panel::Publish);
    }

    /// The footer of the popup: the line inside the frame that lists `needle`, with the box on both sides.
    fn popup_footer_of(text: &str, needle: &str) -> String {
        let line = text
            .lines()
            .filter(|line| line.contains(needle))
            .find(|line| line.contains('\u{2502}'))
            .unwrap_or_else(|| panic!("no popup footer with {needle}\n{text}"));
        line.to_owned()
    }

    #[test]
    fn popups_list_their_keys_in_a_footer_in_the_style_of_the_shortcut_bar() {
        use ratatui::style::{Color, Modifier};
        let (mut state, _directory) = writable_state(&[("cmd", "1")]);

        // Publish dialog: edit stage, then the review.
        press(&mut state, "p");
        handle_key(&mut state, KeyCode::Tab);
        let text = screen(&mut state, 140, 40);
        let footer = popup_footer_of(&text, "[Ctrl+S] Send");
        assert!(
            footer.contains("[Esc] Cancel") && footer.contains("[Ctrl+F] JSON layout"),
            "{footer}"
        );
        assert!(
            footer.contains("[Ctrl+V] Paste") && footer.contains("[Ctrl+U] Clear"),
            "{footer}"
        );
        assert!(
            !text.contains("Enter reviews before sending"),
            "the plain hint is gone"
        );
        let cyan_bold: Vec<_> = styled_cells(&mut state, "S")
            .into_iter()
            .filter(|(color, _)| *color == Some(Color::Cyan))
            .collect();
        assert!(!cyan_bold.is_empty(), "keys are drawn in the accent color");
        let _ = Modifier::BOLD;

        press(&mut state, "x");
        for _ in 0..2 {
            handle_key(&mut state, KeyCode::Tab);
        }
        handle_key(&mut state, KeyCode::Enter);
        let text = screen(&mut state, 140, 40);
        assert_eq!(
            state.publish_editor.as_ref().unwrap().stage,
            publish::PublishStage::Review
        );
        popup_footer_of(&text, "[y/Ctrl+S] Publish");
        assert!(popup_footer_of(&text, "[n/Esc] Back").contains('\u{2502}'));
        handle_key(&mut state, KeyCode::Esc);
        handle_key(&mut state, KeyCode::Esc);

        // Broker editor, with the protocol arrows on that field.
        press(&mut state, "ba");
        let text = screen(&mut state, 140, 40);
        let footer = popup_footer_of(&text, "[Enter] Next/Save");
        assert!(
            footer.contains("[Esc] Cancel") && !footer.contains("Protocol"),
            "{footer}"
        );
        for _ in 0..3 {
            handle_key(&mut state, KeyCode::Tab);
        }
        let text = screen(&mut state, 140, 40);
        popup_footer_of(&text, "[\u{2190}/\u{2192}] Protocol");
        handle_key(&mut state, KeyCode::Esc);

        // Logging dialog and the confirmation that closes the manager's quit question.
        handle_key(&mut state, KeyCode::Esc);
        press(&mut state, "L");
        popup_footer_of(&screen(&mut state, 140, 40), "[Space] On/off");
        handle_key(&mut state, KeyCode::Esc);
        press(&mut state, "q");
        let text = screen(&mut state, 140, 40);
        popup_footer_of(&text, "[y/Enter] Quit");
        assert!(
            !text.contains("[y/Enter] Quit   [n/Esc] Stay"),
            "no second, unstyled copy"
        );
        handle_key(&mut state, KeyCode::Esc);

        // The help overlay.
        press(&mut state, "?");
        popup_footer_of(&screen(&mut state, 140, 40), "[Esc] Close help");
    }

    #[test]
    fn a_remove_confirmation_and_the_certificate_dialog_have_footers_too() {
        let (store, _directory) = temp_store();
        profile(&store, "test");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        state.view = View::Brokers;
        press(&mut state, "d");
        popup_footer_of(&screen(&mut state, 140, 40), "[y] Confirm");
        press(&mut state, "n");

        press(&mut state, "v");
        press(&mut state, "i");
        let text = screen(&mut state, 140, 40);
        assert!(state.certificate_editor.is_some());
        popup_footer_of(&text, "[Enter] Apply");
        handle_key(&mut state, KeyCode::Esc);
    }

    #[test]
    fn a_failing_log_file_switches_logging_off_with_an_explanation() {
        let (store, _directory) = temp_store();
        let mut state = UiState::new(store.load().unwrap(), store, None);
        state.log.enabled = true;
        state.logging_failed("disk full");
        assert!(state.log_directory().is_none());
        let notice = state.notice.as_ref().unwrap();
        assert!(notice.error && notice.text.contains("disk full"));
    }

    #[test]
    fn history_and_info_name_the_broker_of_each_message() {
        let (mut state, _alpha, _beta, _directory) = two_brokers();
        press(&mut state, "jj");
        assert_eq!(state.main.selected.as_deref(), Some("both"));
        press(&mut state, "h");
        let text = screen(&mut state, 140, 24);
        // Newest first: the message of broker 2, then the one of broker 1.
        let markers: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("chg"))
            .filter_map(|line| line.rsplit("\u{2502}\u{2502}").next())
            .map(|cell| cell.trim_start().get(..1).unwrap_or(""))
            .collect();
        assert_eq!(markers, ["2", "1"], "{text}");
        assert!(
            text.contains("\u{b7} 2 Beta"),
            "the inspector header names the broker\n{text}"
        );

        press(&mut state, "i");
        let text = screen(&mut state, 140, 40);
        let broker_row = text
            .lines()
            .find(|line| line.contains("Broker"))
            .expect("a Broker row");
        assert!(
            broker_row.contains("2") && broker_row.contains("Beta"),
            "{text}"
        );
    }

    /// Two recordings in `directory` and a state with Alpha (may publish) and Beta (read-only) connected.
    fn replay_state() -> (UiState, BrokerConfig, BrokerConfig, tempfile::TempDir) {
        let (store, directory) = temp_store();
        let mut alpha = profile(&store, "Alpha");
        alpha.allow_publish = true;
        let beta = profile(&store, "Beta");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        state.begin_connections(&[alpha.clone(), beta.clone()]);
        for broker in [&alpha, &beta] {
            state.apply_event_from(
                &broker.id,
                BrokerEvent::State {
                    state: ConnectionState::Connected,
                    detail: None,
                },
            );
        }
        let logs = directory.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("a_20261023.0.mqttlog"),
            concat!(
                "2026-10-23 10:00:00,000 | T0  I: plant/one [{\"source\":\"plc\"}] {\"a\":1}\n",
                "2026-10-23 10:00:01,000 | T0  I: $SYS/broker/uptime [] 5\n",
                "2026-10-23 10:00:02,000 | T0  I: plant/two [] two\n",
                "2026-10-23 10:00:03,500 | T0  I: other/three [] 3\n",
            ),
        )
        .unwrap();
        std::fs::write(logs.join("b_20261023.0.mqttlog"), "garbage only\n").unwrap();
        state.log.directory = logs.to_str().unwrap().to_owned();
        (state, alpha, beta, directory)
    }

    #[test]
    fn replay_loads_recordings_asks_once_and_plays_to_a_writable_broker() {
        let (mut state, alpha, beta, _directory) = replay_state();
        press(&mut state, "R");
        assert_eq!(state.view, View::Replay);
        let text = screen(&mut state, 120, 20);
        assert!(
            text.contains("a_20261023.0.mqttlog") && text.contains("b_20261023.0.mqttlog"),
            "{text}"
        );

        // A file without messages is refused and explained.
        press(&mut state, "j");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.replay.stage, replay::Stage::Files);
        assert!(state.replay.error.as_ref().unwrap().contains("No messages"));

        press(&mut state, "k");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.replay.stage, replay::Stage::Player);
        let player = state.replay.player.as_ref().unwrap();
        assert_eq!(player.progress(), (0, 3), "the $SYS message is left out");
        assert_eq!(state.replay.target.as_deref(), Some(alpha.id.as_str()));
        let text = screen(&mut state, 120, 24);
        assert!(
            text.contains("PAUSED") && text.contains("Alpha") && text.contains("not sendable"),
            "{text}"
        );

        // Nothing leaves before the question is answered.
        let later = Instant::now() + Duration::from_secs(30);
        press(&mut state, " ");
        assert!(state.replay.confirming);
        assert!(screen(&mut state, 120, 24).contains("Send 3 messages to Alpha?"));
        assert!(state.replay_batch(later).is_none());
        press(&mut state, "n");
        assert!(!state.replay.confirming && !state.replay.is_playing());

        press(&mut state, " ");
        press(&mut state, "y");
        assert!(state.replay.is_playing());
        assert!(state.replay_status().unwrap().starts_with("REPLAY"));
        let (broker, requests) = state.replay_batch(later).expect("everything is due");
        assert_eq!(broker, alpha.id);
        let topics: Vec<&str> = requests.iter().map(|r| r.topic.as_str()).collect();
        assert_eq!(topics, ["plant/one", "plant/two", "other/three"]);
        assert_eq!(requests[0].payload, br#"{"a":1}"#);
        assert_eq!(
            requests[0].user_properties,
            [("source".to_owned(), "plc".to_owned())]
        );
        assert!(requests.iter().all(|r| !r.retain && r.qos == 0));

        // The target cannot be a broker that forbids publishing.
        assert!(state.publishable_brokers().all(|c| c.broker_id != beta.id));
    }

    #[test]
    fn replay_pauses_changes_pacing_and_switches_the_target() {
        let (mut state, alpha, mut beta, _directory) = replay_state();
        beta.allow_publish = true;
        state.begin_connections(&[alpha.clone(), beta.clone()]);
        for broker in [&alpha, &beta] {
            state.apply_event_from(
                &broker.id,
                BrokerEvent::State {
                    state: ConnectionState::Connected,
                    detail: None,
                },
            );
        }
        press(&mut state, "R");
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, "b");
        assert_eq!(state.replay.target.as_deref(), Some(beta.id.as_str()));
        press(&mut state, " y");

        // Pause with Space; a paused replay hands out nothing.
        let soon = Instant::now() + Duration::from_millis(1);
        assert_eq!(
            state.replay_batch(soon).unwrap().1.len(),
            1,
            "the first message is due at once"
        );
        press(&mut state, " ");
        assert!(!state.replay.is_playing());
        assert!(
            state
                .replay_batch(Instant::now() + Duration::from_secs(60))
                .is_none()
        );
        press(&mut state, " ");
        assert!(
            state.replay.is_playing(),
            "resuming needs no second question"
        );

        // Speed, mode, loop, QoS and filter.
        let speed = state.replay.settings.speed;
        press(&mut state, "+");
        assert_eq!(state.replay.settings.speed, speed + 1);
        press(&mut state, "-");
        press(&mut state, "-");
        assert_eq!(state.replay.settings.speed, speed - 1);
        press(&mut state, "m");
        assert_eq!(
            state.replay.settings.pacing,
            mate::replay::Pacing::FixedRate
        );
        press(&mut state, "m");
        press(&mut state, "l2");
        assert!(state.replay.settings.looping && state.replay.settings.qos == 2);
        press(&mut state, "/");
        press(&mut state, "other/#");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.replay.player.as_ref().unwrap().progress().1, 1);
        press(&mut state, "/");
        press(&mut state, "a/#/b");
        handle_key(&mut state, KeyCode::Enter);
        assert!(state.replay.error.as_ref().unwrap().contains("not a valid"));

        // Changing the target asks again.
        press(&mut state, "b");
        assert!(!state.replay.is_playing() && !state.replay.armed);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.replay.stage, replay::Stage::Files);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.view, View::Main);
    }

    #[test]
    fn replay_needs_a_broker_that_allows_publishing() {
        let (store, directory) = temp_store();
        let alpha = profile(&store, "Alpha");
        let mut state = UiState::new(store.load().unwrap(), store, None);
        connected(&mut state, &alpha);
        let logs = directory.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("a.mqttlog"),
            "2026-10-23 10:00:00,000 | T0  I: t [] x\n",
        )
        .unwrap();
        state.log.directory = logs.to_str().unwrap().to_owned();

        press(&mut state, "R");
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.replay.target, None);
        press(&mut state, " ");
        assert!(!state.replay.confirming);
        assert!(
            state
                .replay
                .error
                .as_ref()
                .unwrap()
                .contains("allows publishing")
        );
        press(&mut state, "b");
        assert!(
            state
                .replay
                .error
                .as_ref()
                .unwrap()
                .contains("allow publishing")
        );
    }

    #[test]
    fn a_busy_connection_retries_other_failures_pause() {
        let (mut state, _alpha, _beta, _directory) = replay_state();
        press(&mut state, "R");
        handle_key(&mut state, KeyCode::Enter);
        press(&mut state, " y");
        let later = Instant::now() + Duration::from_secs(30);
        let (_, requests) = state.replay_batch(later).unwrap();
        assert_eq!(requests.len(), 3);

        // The last two did not fit into the queue: they come again, the replay goes on.
        state.replay_failed(2, &anyhow::Error::new(mate::mqtt::QueueFull("full".into())));
        assert!(state.replay.is_playing());
        let (_, again) = state.replay_batch(later).unwrap();
        assert_eq!(
            again[0].topic, "plant/two",
            "the first unsent one comes first"
        );
        let (_, rest) = state.replay_batch(later + Duration::from_secs(10)).unwrap();
        assert_eq!(rest[0].topic, "other/three", "the recorded spacing is kept");

        state.replay_failed(1, &anyhow::anyhow!("not connected"));
        assert!(!state.replay.is_playing());
        let notice = state.notice.as_ref().unwrap();
        assert!(notice.error && notice.text.contains("Replay paused"));
    }

    #[test]
    fn a_typed_path_opens_a_folder_or_a_file() {
        let (mut state, _alpha, _beta, directory) = replay_state();
        press(&mut state, "R");
        press(&mut state, "p");
        for _ in 0..200 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(
            &mut state,
            directory
                .path()
                .join("logs/a_20261023.0.mqttlog")
                .to_str()
                .unwrap(),
        );
        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.replay.stage, replay::Stage::Player);
        handle_key(&mut state, KeyCode::Esc);

        press(&mut state, "p");
        for _ in 0..200 {
            handle_key(&mut state, KeyCode::Backspace);
        }
        press(&mut state, "/nonexistent/folder");
        handle_key(&mut state, KeyCode::Enter);
        assert!(
            state
                .replay
                .error
                .as_ref()
                .unwrap()
                .contains("does not exist")
        );
    }
}
