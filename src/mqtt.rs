use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, MqttOptions, Outgoing, Packet, QoS,
    Transport,
    v5::{
        self,
        mqttbytes::{
            QoS as QoS5,
            v5::{ConnectReturnCode as ConnectReturnCode5, Packet as Packet5},
        },
    },
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};

use crate::{
    certificates::validate_tls_profile,
    config::{BrokerConfig, SYSTEM_TOPICS},
    security::{broker_credentials, tls_client_config},
    topics::{MessageProperties, topic_matches_filter, valid_topic_filter},
};

const EVENT_CAPACITY: usize = 256;
const MAX_PACKET_SIZE: usize = 1_048_576;

pub const PROTOCOL_MQTT311: &str = "mqtt311";
pub const PROTOCOL_MQTT5: &str = "mqtt5";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    /// The connection dropped or an attempt failed; the task keeps retrying.
    Reconnecting,
    Disconnected,
    /// Terminal failure; the broker task has stopped.
    Error,
}

#[derive(Debug)]
pub struct IncomingMessage {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    pub dup: bool,
    /// Only MQTT 5 messages can carry properties.
    pub properties: Option<Arc<MessageProperties>>,
    pub received: Instant,
    pub wall: SystemTime,
}

#[derive(Debug)]
pub enum BrokerEvent {
    State {
        state: ConnectionState,
        detail: Option<String>,
    },
    /// Informational message that does not change the connection state.
    Notice(String),
    /// Terminal failure; the broker task has stopped and will not retry.
    Failed(String),
    Message(IncomingMessage),
}

type SubscriptionList = Arc<Mutex<Vec<(String, u8)>>>;
/// Topic filters whose messages are dropped on arrival.
type ExclusionList = Arc<Mutex<Vec<String>>>;

/// The connection's request queue is full: the message was not queued and may be offered again.
#[derive(Debug)]
pub struct QueueFull(pub String);

impl std::fmt::Display for QueueFull {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "could not queue the message: {}", self.0)
    }
}

impl std::error::Error for QueueFull {}

pub struct BrokerRuntime {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    client: Arc<OnceLock<Client>>,
    connected: Arc<AtomicBool>,
    allow_publish: bool,
    subscriptions: SubscriptionList,
    exclusions: ExclusionList,
    client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    /// Sent with MQTT 5 connections only; MQTT 3.1.1 has no user properties.
    pub user_properties: Vec<(String, String)>,
}

/// Leaves room for the packet header inside the connection's packet size limit.
pub const MAX_PUBLISH_PAYLOAD: usize = MAX_PACKET_SIZE - 4096;

pub fn validate_publish(request: &PublishRequest) -> Result<()> {
    ensure!(!request.topic.is_empty(), "the topic must not be empty");
    ensure!(
        request.topic.len() <= 65_535,
        "the topic is longer than 65,535 bytes"
    );
    ensure!(
        !request.topic.contains(['+', '#', '\0']),
        "the topic must not contain wildcards (+ #) or NUL"
    );
    ensure!(
        !request.topic.starts_with('$'),
        "topics starting with $ are reserved for the broker"
    );
    ensure!(request.qos <= 2, "QoS must be 0, 1, or 2");
    ensure!(
        request.payload.len() <= MAX_PUBLISH_PAYLOAD,
        "the payload is larger than {MAX_PUBLISH_PAYLOAD} bytes"
    );
    Ok(())
}

impl BrokerRuntime {
    /// The client ID this connection uses, including the random suffix if the profile asks for one.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Queues a message on the live connection. Refused unless the profile allows publishing,
    /// so no front end can write to a broker the user did not explicitly enable.
    pub fn publish(&self, request: PublishRequest) -> Result<()> {
        ensure!(
            self.allow_publish,
            "publishing is disabled for this broker (enable it with `mate broker edit <id> --allow-publish true`)"
        );
        validate_publish(&request)?;
        let client = self.live_client("the message was not sent")?;
        client
            .try_publish(&request)
            .map_err(|error| anyhow::Error::new(QueueFull(error)))
    }

    /// Subscribes on the live connection and remembers the filter for reconnects.
    pub fn subscribe(&self, topic: &str, qos_level: u8) -> Result<()> {
        ensure!(
            valid_topic_filter(topic),
            "invalid MQTT topic filter: {topic}"
        );
        ensure!(qos_level <= 2, "subscription QoS must be 0, 1, or 2");
        {
            let mut list = self.subscriptions.lock().expect("subscription list");
            match list.iter_mut().find(|(known, _)| known == topic) {
                Some(entry) => entry.1 = qos_level,
                None => list.push((topic.to_owned(), qos_level)),
            }
        }
        match self.live_client("the subscription applies on the next connection") {
            Ok(client) => client
                .try_subscribe(topic, qos_level)
                .map_err(|error| anyhow!("could not queue the subscription: {error}")),
            // Not connected: the list above is used when the connection comes up.
            Err(_) => Ok(()),
        }
    }

    /// Starts dropping messages that match `filter`; takes effect at once and for reconnects.
    pub fn exclude(&self, filter: &str) -> Result<()> {
        ensure!(
            valid_topic_filter(filter),
            "invalid MQTT topic filter: {filter}"
        );
        let mut list = self.exclusions.lock().expect("exclusion list");
        if !list.iter().any(|known| known == filter) {
            list.push(filter.to_owned());
        }
        Ok(())
    }

    pub fn unexclude(&self, filter: &str) {
        self.exclusions
            .lock()
            .expect("exclusion list")
            .retain(|known| known != filter);
    }

    pub fn unsubscribe(&self, topic: &str) -> Result<()> {
        let was_subscribed = {
            let mut list = self.subscriptions.lock().expect("subscription list");
            let before = list.len();
            list.retain(|(known, _)| known != topic);
            list.len() < before
        };
        if !was_subscribed {
            return Ok(());
        }
        match self.live_client("") {
            Ok(client) => client
                .try_unsubscribe(topic)
                .map_err(|error| anyhow!("could not queue the unsubscribe: {error}")),
            Err(_) => Ok(()),
        }
    }

    fn live_client(&self, consequence: &str) -> Result<&Client> {
        ensure!(
            self.connected.load(Ordering::Relaxed),
            "not connected; {consequence}"
        );
        self.client
            .get()
            .with_context(|| format!("not connected; {consequence}"))
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let mut task = self.task;
        if time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

/// The client handle of either protocol version.
#[derive(Clone)]
enum Client {
    V311(AsyncClient),
    V5(v5::AsyncClient),
}

enum Options {
    V311(Box<MqttOptions>),
    V5(Box<v5::MqttOptions>),
}

enum EventSource {
    V311(Box<rumqttc::EventLoop>),
    V5(Box<v5::EventLoop>),
}

/// Protocol-independent view of what the event loop reported.
enum Polled {
    Connected,
    Message(IncomingMessage),
    DisconnectSent,
    Ignored,
    AuthFailed(String),
    Failed(String),
}

impl Client {
    async fn subscribe(&self, topic: &str, level: u8) -> Result<(), String> {
        match self {
            Self::V311(client) => client
                .subscribe(topic, qos(level))
                .await
                .map_err(|error| error.to_string()),
            Self::V5(client) => client
                .subscribe(topic, qos5(level))
                .await
                .map_err(|error| error.to_string()),
        }
    }

    fn try_subscribe(&self, topic: &str, level: u8) -> Result<(), String> {
        match self {
            Self::V311(client) => client
                .try_subscribe(topic, qos(level))
                .map_err(|error| error.to_string()),
            Self::V5(client) => client
                .try_subscribe(topic, qos5(level))
                .map_err(|error| error.to_string()),
        }
    }

    fn try_unsubscribe(&self, topic: &str) -> Result<(), String> {
        match self {
            Self::V311(client) => client
                .try_unsubscribe(topic)
                .map_err(|error| error.to_string()),
            Self::V5(client) => client
                .try_unsubscribe(topic)
                .map_err(|error| error.to_string()),
        }
    }

    fn try_publish(&self, request: &PublishRequest) -> Result<(), String> {
        let payload = request.payload.clone();
        match self {
            Self::V311(client) => client
                .try_publish(&request.topic, qos(request.qos), request.retain, payload)
                .map_err(|error| error.to_string()),
            Self::V5(client) if request.user_properties.is_empty() => client
                .try_publish(&request.topic, qos5(request.qos), request.retain, payload)
                .map_err(|error| error.to_string()),
            Self::V5(client) => client
                .try_publish_with_properties(
                    &request.topic,
                    qos5(request.qos),
                    request.retain,
                    payload,
                    v5::mqttbytes::v5::PublishProperties {
                        user_properties: request.user_properties.clone(),
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string()),
        }
    }

    async fn disconnect(&self) {
        match self {
            Self::V311(client) => {
                let _ = client.disconnect().await;
            }
            Self::V5(client) => {
                let _ = client.disconnect().await;
            }
        }
    }
}

impl Options {
    fn open(self) -> (Client, EventSource) {
        match self {
            Self::V311(options) => {
                let (client, event_loop) = AsyncClient::new(*options, EVENT_CAPACITY);
                (
                    Client::V311(client),
                    EventSource::V311(Box::new(event_loop)),
                )
            }
            Self::V5(options) => {
                let (client, event_loop) = v5::AsyncClient::new(*options, EVENT_CAPACITY);
                (Client::V5(client), EventSource::V5(Box::new(event_loop)))
            }
        }
    }
}

impl EventSource {
    async fn poll(&mut self) -> Polled {
        match self {
            Self::V311(event_loop) => match event_loop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => Polled::Connected,
                Ok(Event::Incoming(Packet::Publish(publish))) => Polled::Message(IncomingMessage {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                    qos: publish.qos as u8,
                    retain: publish.retain,
                    dup: publish.dup,
                    properties: None,
                    received: Instant::now(),
                    wall: SystemTime::now(),
                }),
                Ok(Event::Outgoing(Outgoing::Disconnect)) => Polled::DisconnectSent,
                Ok(_) => Polled::Ignored,
                Err(ConnectionError::ConnectionRefused(code)) if is_auth_failure(code) => {
                    Polled::AuthFailed(format!("{code:?}"))
                }
                Err(error) => Polled::Failed(error.to_string()),
            },
            Self::V5(event_loop) => match event_loop.poll().await {
                Ok(v5::Event::Incoming(Packet5::ConnAck(_))) => Polled::Connected,
                Ok(v5::Event::Incoming(Packet5::Publish(publish))) => {
                    Polled::Message(IncomingMessage {
                        topic: String::from_utf8_lossy(&publish.topic).into_owned(),
                        payload: publish.payload.to_vec(),
                        qos: publish.qos as u8,
                        retain: publish.retain,
                        dup: publish.dup,
                        properties: publish.properties.map(message_properties),
                        received: Instant::now(),
                        wall: SystemTime::now(),
                    })
                }
                Ok(v5::Event::Outgoing(Outgoing::Disconnect)) => Polled::DisconnectSent,
                Ok(_) => Polled::Ignored,
                Err(v5::ConnectionError::ConnectionRefused(code)) if is_auth_failure5(code) => {
                    Polled::AuthFailed(format!("{code:?}"))
                }
                Err(error) => Polled::Failed(error.to_string()),
            },
        }
    }
}

fn message_properties(properties: v5::mqttbytes::v5::PublishProperties) -> Arc<MessageProperties> {
    Arc::new(MessageProperties {
        payload_is_utf8: properties.payload_format_indicator.map(|flag| flag == 1),
        message_expiry_secs: properties.message_expiry_interval,
        response_topic: properties.response_topic,
        correlation_data: properties.correlation_data.map(|data| data.to_vec()),
        user_properties: properties.user_properties,
        subscription_ids: properties.subscription_identifiers,
        content_type: properties.content_type,
    })
}

fn enabled_subscriptions(profile: &BrokerConfig) -> Vec<(String, u8)> {
    let mut wanted: Vec<(String, u8)> = profile
        .subscriptions
        .iter()
        .filter(|subscription| subscription.enabled && !subscription.exclude)
        .map(|subscription| (subscription.topic.clone(), subscription.qos))
        .collect();
    if profile.show_system_topics && !wanted.iter().any(|(topic, _)| topic == SYSTEM_TOPICS) {
        wanted.push((SYSTEM_TOPICS.to_owned(), 0));
    }
    wanted
}

fn enabled_exclusions(profile: &BrokerConfig) -> Vec<String> {
    profile
        .subscriptions
        .iter()
        .filter(|subscription| subscription.enabled && subscription.exclude)
        .map(|subscription| subscription.topic.clone())
        .collect()
}

fn is_excluded(exclusions: &ExclusionList, topic: &str) -> bool {
    exclusions
        .lock()
        .expect("exclusion list")
        .iter()
        .any(|filter| topic_matches_filter(filter, topic))
}

pub fn spawn_broker(
    profile: BrokerConfig,
    event_tx: mpsc::Sender<BrokerEvent>,
) -> Result<(BrokerRuntime, Arc<AtomicU64>)> {
    validate_profile(&profile)?;
    let client_id = effective_client_id(&profile);

    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let dropped_messages = Arc::new(AtomicU64::new(0));
    let dropped_for_task = Arc::clone(&dropped_messages);
    let client_cell = Arc::new(OnceLock::new());
    let connected = Arc::new(AtomicBool::new(false));
    let allow_publish = profile.allow_publish;
    let subscriptions: SubscriptionList = Arc::new(Mutex::new(enabled_subscriptions(&profile)));
    let exclusions: ExclusionList = Arc::new(Mutex::new(enabled_exclusions(&profile)));
    let (task_client_cell, task_connected, task_subscriptions, task_exclusions) = (
        Arc::clone(&client_cell),
        Arc::clone(&connected),
        Arc::clone(&subscriptions),
        Arc::clone(&exclusions),
    );
    let task_client_id = client_id.clone();

    let task = tokio::spawn(async move {
        send_state(&event_tx, ConnectionState::Connecting, None);
        let setup_profile = profile.clone();
        let setup_client_id = task_client_id.clone();
        let options = match tokio::task::spawn_blocking(move || {
            build_mqtt_options(&setup_profile, &setup_client_id)
        })
        .await
        {
            Ok(Ok(options)) => options,
            Ok(Err(error)) => {
                let _ = event_tx
                    .send(BrokerEvent::Failed(format!("{error:#}")))
                    .await;
                return;
            }
            Err(error) => {
                let message = format!("Broker setup task failed: {error}");
                let _ = event_tx.send(BrokerEvent::Failed(message)).await;
                return;
            }
        };
        let (client, mut event_loop) = options.open();
        let _ = task_client_cell.set(client.clone());
        let mut reconnect_delay = Duration::from_secs(1);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        client.disconnect().await;
                        let _ = time::timeout(Duration::from_secs(1), async {
                            loop {
                                match event_loop.poll().await {
                                    Polled::DisconnectSent | Polled::Failed(_) | Polled::AuthFailed(_) => break,
                                    _ => {}
                                }
                            }
                        })
                        .await;
                        break;
                    }
                }
                polled = event_loop.poll() => match polled {
                    Polled::Connected => {
                        reconnect_delay = Duration::from_secs(1);
                        task_connected.store(true, Ordering::Relaxed);
                        send_state(&event_tx, ConnectionState::Connected, None);
                        // Brokers drop subscriptions of clean sessions on reconnect.
                        let wanted = task_subscriptions.lock().expect("subscription list").clone();
                        for (topic, level) in wanted {
                            if let Err(error) = client.subscribe(&topic, level).await {
                                let _ = event_tx.try_send(BrokerEvent::Notice(format!(
                                    "Subscription request failed: {error}"
                                )));
                            }
                        }
                    }
                    Polled::Message(message) => {
                        if is_excluded(&task_exclusions, &message.topic) {
                            // The broker sent it, but the user asked not to see it.
                        } else if event_tx.try_send(BrokerEvent::Message(message)).is_err() {
                            dropped_for_task.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Polled::Ignored | Polled::DisconnectSent => {}
                    Polled::AuthFailed(code) => {
                        task_connected.store(false, Ordering::Relaxed);
                        let message = format!(
                            "Authentication failed ({code}) as '{}' with client ID '{}'; not retrying",
                            profile.auth.username.as_deref().unwrap_or("(no user)"),
                            task_client_id
                        );
                        let _ = event_tx.send(BrokerEvent::Failed(message)).await;
                        break;
                    }
                    Polled::Failed(error) => {
                        task_connected.store(false, Ordering::Relaxed);
                        send_state(&event_tx, ConnectionState::Reconnecting, Some(error));
                        time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
    });

    Ok((
        BrokerRuntime {
            shutdown,
            task,
            client: client_cell,
            connected,
            allow_publish,
            subscriptions,
            exclusions,
            client_id,
        },
        dropped_messages,
    ))
}

/// A fixed client ID makes the broker drop the older of two sessions, so each connection gets its own.
fn effective_client_id(profile: &BrokerConfig) -> String {
    if !profile.unique_client_id {
        return profile.client_id.clone();
    }
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    format!("{}-{:06x}", profile.client_id, hasher.finish() & 0xff_ffff)
}

fn build_mqtt_options(profile: &BrokerConfig, client_id: &str) -> Result<Options> {
    validate_tls_profile(profile)?;
    let credentials = broker_credentials(profile)?;
    let transport = tls_client_config(profile)?
        .map(|tls_config| Transport::tls_with_config((*tls_config).clone().into()));
    if profile.protocol == PROTOCOL_MQTT5 {
        let mut options = v5::MqttOptions::new(client_id, &profile.host, profile.port);
        options
            .set_keep_alive(Duration::from_secs(30))
            .set_max_packet_size(Some(MAX_PACKET_SIZE as u32));
        if let Some((user, password)) = credentials {
            options.set_credentials(user, password);
        }
        if let Some(transport) = transport {
            options.set_transport(transport);
        }
        return Ok(Options::V5(Box::new(options)));
    }
    let mut options = MqttOptions::new(client_id, &profile.host, profile.port);
    options
        .set_keep_alive(Duration::from_secs(30))
        .set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE);
    if let Some((user, password)) = credentials {
        options.set_credentials(user, password);
    }
    if let Some(transport) = transport {
        options.set_transport(transport);
    }
    Ok(Options::V311(Box::new(options)))
}

fn send_state(
    event_tx: &mpsc::Sender<BrokerEvent>,
    state: ConnectionState,
    detail: Option<String>,
) {
    let _ = event_tx.try_send(BrokerEvent::State { state, detail });
}

/// One-shot connectivity check used by `broker test` and the TUI; stops after the first ConnAck.
pub async fn test_connection(profile: BrokerConfig, timeout: Duration) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let (runtime, _) = spawn_broker(profile, event_tx)?;
    let result = time::timeout(timeout, async {
        loop {
            match event_rx.recv().await {
                Some(BrokerEvent::Failed(message)) => bail!("{message}"),
                Some(BrokerEvent::State {
                    state: ConnectionState::Connected,
                    ..
                }) => return Ok(()),
                Some(BrokerEvent::State {
                    state: ConnectionState::Reconnecting,
                    detail,
                }) => bail!(detail.unwrap_or_else(|| "connection failed".to_owned())),
                Some(_) => {}
                None => bail!("broker task stopped before connecting"),
            }
        }
    })
    .await;
    runtime.stop().await;
    match result {
        Ok(outcome) => outcome,
        Err(_) => bail!("connection timed out after {} seconds", timeout.as_secs()),
    }
}

fn validate_profile(profile: &BrokerConfig) -> Result<()> {
    ensure!(
        matches!(profile.protocol.as_str(), PROTOCOL_MQTT311 | PROTOCOL_MQTT5),
        "unsupported protocol '{}': use {PROTOCOL_MQTT311} or {PROTOCOL_MQTT5}",
        profile.protocol
    );
    ensure!(
        profile.auth.username.is_some() == profile.auth.password_ref.is_some(),
        "broker username and password_ref must be configured together"
    );
    if let Some(reference) = &profile.auth.password_ref {
        ensure!(
            reference.starts_with("keyring:"),
            "plaintext passwords are not supported; use a keyring: reference"
        );
    }
    for subscription in &profile.subscriptions {
        ensure!(
            valid_topic_filter(&subscription.topic),
            "invalid MQTT topic filter: {}",
            subscription.topic
        );
        ensure!(subscription.qos <= 2, "subscription QoS must be 0, 1, or 2");
    }
    if profile.host.trim().is_empty() {
        bail!("broker hostname must not be empty");
    }
    Ok(())
}

fn is_auth_failure(code: ConnectReturnCode) -> bool {
    matches!(
        code,
        ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized
    )
}

fn is_auth_failure5(code: ConnectReturnCode5) -> bool {
    matches!(
        code,
        ConnectReturnCode5::BadUserNamePassword
            | ConnectReturnCode5::NotAuthorized
            | ConnectReturnCode5::BadAuthenticationMethod
    )
}

fn qos(value: u8) -> QoS {
    match value {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => unreachable!("validated QoS is in the 0..=2 range"),
    }
}

fn qos5(value: u8) -> QoS5 {
    match value {
        0 => QoS5::AtMostOnce,
        1 => QoS5::AtLeastOnce,
        2 => QoS5::ExactlyOnce,
        _ => unreachable!("validated QoS is in the 0..=2 range"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_rejects_unsupported_protocols_but_allows_tls_profiles() {
        let mut profile = BrokerConfig::default();
        assert!(validate_profile(&profile).is_ok());

        profile.tls.enabled = true;
        assert!(validate_profile(&profile).is_ok());

        profile.protocol = "mqtt5".to_owned();
        assert!(validate_profile(&profile).is_ok());

        profile.protocol = "mqtt4".to_owned();
        assert!(
            validate_profile(&profile)
                .unwrap_err()
                .to_string()
                .contains("unsupported protocol")
        );
    }

    fn request(topic: &str) -> PublishRequest {
        PublishRequest {
            topic: topic.to_owned(),
            payload: b"x".to_vec(),
            qos: 0,
            retain: false,
            user_properties: Vec::new(),
        }
    }

    #[test]
    fn publish_requests_are_validated() {
        assert!(validate_publish(&request("plant/line-1/cmd")).is_ok());
        for bad in ["", "plant/+/cmd", "plant/#", "$SYS/broker", "a\0b"] {
            assert!(validate_publish(&request(bad)).is_err(), "{bad:?}");
        }
        let mut bad_qos = request("a");
        bad_qos.qos = 3;
        assert!(validate_publish(&bad_qos).is_err());
        let mut large = request("a");
        large.payload = vec![0; MAX_PUBLISH_PAYLOAD + 1];
        assert!(validate_publish(&large).is_err());
        large.payload.truncate(MAX_PUBLISH_PAYLOAD);
        assert!(validate_publish(&large).is_ok());
    }

    #[tokio::test]
    async fn runtime_refuses_to_publish_without_permission_or_connection() {
        let (event_tx, _events) = mpsc::channel(16);
        let mut profile = BrokerConfig {
            host: "127.0.0.1".to_owned(),
            port: 1,
            ..BrokerConfig::default()
        };
        let (runtime, _) = spawn_broker(profile.clone(), event_tx.clone()).unwrap();
        let error = runtime.publish(request("a")).unwrap_err().to_string();
        assert!(error.contains("disabled"), "{error}");
        runtime.stop().await;

        profile.allow_publish = true;
        let (runtime, _) = spawn_broker(profile, event_tx).unwrap();
        let error = runtime.publish(request("a")).unwrap_err().to_string();
        assert!(error.contains("not connected"), "{error}");
        assert!(runtime.publish(request("a/+")).is_err());
        runtime.stop().await;
    }

    #[test]
    fn connector_rejects_invalid_saved_subscription() {
        let mut profile = BrokerConfig::default();
        profile
            .subscriptions
            .push(crate::config::SubscriptionConfig::new("plant/#/state", 0));
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn client_ids_are_unique_per_connection_unless_fixed() {
        let mut profile = BrokerConfig {
            client_id: "mate-prod".to_owned(),
            ..BrokerConfig::default()
        };
        let first = effective_client_id(&profile);
        let second = effective_client_id(&profile);
        assert!(first.starts_with("mate-prod-") && first.len() == "mate-prod-".len() + 6);
        assert_ne!(first, second);

        profile.unique_client_id = false;
        assert_eq!(effective_client_id(&profile), "mate-prod");
    }

    #[test]
    fn system_topics_are_subscribed_only_on_request_and_never_twice() {
        let mut profile = BrokerConfig::default();
        profile
            .subscriptions
            .push(crate::config::SubscriptionConfig::new("plant/#", 1));
        profile
            .subscriptions
            .push(crate::config::SubscriptionConfig::exclusion("+/hmi/#"));
        assert_eq!(enabled_subscriptions(&profile), [("plant/#".to_owned(), 1)]);

        profile.show_system_topics = true;
        let wanted = enabled_subscriptions(&profile);
        assert_eq!(wanted.len(), 2);
        assert_eq!(wanted[1], ("$SYS/#".to_owned(), 0));

        profile
            .subscriptions
            .push(crate::config::SubscriptionConfig::new("$SYS/#", 1));
        assert_eq!(
            enabled_subscriptions(&profile)
                .iter()
                .filter(|(topic, _)| topic == "$SYS/#")
                .count(),
            1,
            "an explicit subscription is not doubled"
        );
        assert!(
            !BrokerConfig::default().show_system_topics,
            "off by default"
        );
    }
}
