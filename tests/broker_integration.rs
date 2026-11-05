//! Needs a real Mosquitto; skipped unless MATE_TEST_BROKER_HOST is set.
//!
//! MATE_TEST_PLAIN_PORT (default 1883), MATE_TEST_TLS_PORT + MATE_TEST_CA,
//! MATE_TEST_MTLS_PORT + MATE_TEST_CA + MATE_TEST_CLIENT_CERT + MATE_TEST_CLIENT_KEY.

use std::{env, path::PathBuf, time::Duration};

use mate::{
    config::{BrokerConfig, SubscriptionConfig},
    mqtt::{BrokerEvent, ConnectionState, PublishRequest, spawn_broker},
};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::{sync::mpsc, time::timeout};

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// Publishes a retained message over the plain listener so the subscriber under test receives it on subscribe.
async fn publish_retained(host: &str, topic: &str, payload: &[u8]) {
    let port = var("MATE_TEST_PLAIN_PORT").map_or(1883, |port| port.parse().unwrap());
    let mut options = MqttOptions::new(format!("mate-it-pub-{}", unique()), host, port);
    options.set_keep_alive(Duration::from_secs(5));
    let (client, mut event_loop) = AsyncClient::new(options, 10);
    client
        .publish(topic, QoS::AtLeastOnce, true, payload.to_vec())
        .await
        .unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if let Event::Incoming(Packet::PubAck(_)) = event_loop.poll().await.unwrap() {
                break;
            }
        }
    })
    .await
    .expect("publisher did not receive PUBACK");
}

async fn receive_retained(
    host: &str,
    port: u16,
    tls: Option<(String, Option<(String, String)>)>,
) -> Vec<u8> {
    let id = unique();
    let topic = format!("mate-it/{id}/status");
    publish_retained(host, &topic, b"RUNNING").await;

    let mut profile = BrokerConfig {
        id: "integration".to_owned(),
        name: "integration".to_owned(),
        host: host.to_owned(),
        port,
        client_id: format!("mate-it-{id}"),
        subscriptions: vec![SubscriptionConfig::new(format!("mate-it/{id}/#"), 1)],
        ..BrokerConfig::default()
    };
    if let Some((ca, client)) = tls {
        profile.tls.enabled = true;
        profile.tls.ca_file = Some(PathBuf::from(ca));
        if let Some((cert, key)) = client {
            profile.tls.client_cert = Some(PathBuf::from(cert));
            profile.tls.client_key = Some(PathBuf::from(key));
        }
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (runtime, _) = spawn_broker(profile, event_tx).unwrap();
    let received = timeout(Duration::from_secs(15), async {
        loop {
            match event_rx.recv().await.expect("broker task ended") {
                BrokerEvent::Message(message) if message.topic == topic => {
                    return message.payload;
                }
                BrokerEvent::Failed(message) => panic!("connection failed: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("retained message was not received");
    runtime.stop().await;
    received
}

#[tokio::test]
async fn plain_connection_receives_retained_message() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let port = var("MATE_TEST_PLAIN_PORT").map_or(1883, |port| port.parse().unwrap());
    assert_eq!(receive_retained(&host, port, None).await, b"RUNNING");
}

#[tokio::test]
async fn tls_connection_receives_retained_message() {
    let (Some(host), Some(port), Some(ca)) = (
        var("MATE_TEST_BROKER_HOST"),
        var("MATE_TEST_TLS_PORT"),
        var("MATE_TEST_CA"),
    ) else {
        eprintln!("skipped: TLS test environment not set");
        return;
    };
    let payload = receive_retained(&host, port.parse().unwrap(), Some((ca, None))).await;
    assert_eq!(payload, b"RUNNING");
}

#[tokio::test]
async fn mtls_connection_receives_retained_message() {
    let (Some(host), Some(port), Some(ca), Some(cert), Some(key)) = (
        var("MATE_TEST_BROKER_HOST"),
        var("MATE_TEST_MTLS_PORT"),
        var("MATE_TEST_CA"),
        var("MATE_TEST_CLIENT_CERT"),
        var("MATE_TEST_CLIENT_KEY"),
    ) else {
        eprintln!("skipped: mTLS test environment not set");
        return;
    };
    let payload =
        receive_retained(&host, port.parse().unwrap(), Some((ca, Some((cert, key))))).await;
    assert_eq!(payload, b"RUNNING");
}

async fn wait_until_connected(events: &mut mpsc::Receiver<BrokerEvent>) {
    timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await.expect("broker task ended") {
                BrokerEvent::State {
                    state: ConnectionState::Connected,
                    ..
                } => return,
                BrokerEvent::Failed(message) => panic!("connection failed: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("did not connect");
}

#[tokio::test]
async fn publishing_reaches_subscribers_and_needs_the_profile_permission() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let port = var("MATE_TEST_PLAIN_PORT").map_or(1883, |port| port.parse().unwrap());
    let id = unique();
    let topic = format!("mate-it/{id}/cmd");
    let profile = |role: &str, allow_publish: bool| BrokerConfig {
        id: role.to_owned(),
        name: role.to_owned(),
        host: host.clone(),
        port,
        client_id: format!("mate-it-{role}-{id}"),
        allow_publish,
        subscriptions: vec![SubscriptionConfig::new(format!("mate-it/{id}/#"), 1)],
        ..BrokerConfig::default()
    };

    let (subscriber_tx, mut subscriber_rx) = mpsc::channel(64);
    let (subscriber, _) = spawn_broker(profile("subscriber", false), subscriber_tx).unwrap();
    wait_until_connected(&mut subscriber_rx).await;

    let (publisher_tx, mut publisher_rx) = mpsc::channel(64);
    let (publisher, _) = spawn_broker(profile("publisher", true), publisher_tx).unwrap();
    wait_until_connected(&mut publisher_rx).await;
    publisher
        .publish(PublishRequest {
            topic: topic.clone(),
            payload: b"hello".to_vec(),
            qos: 1,
            retain: false,
            user_properties: Vec::new(),
        })
        .unwrap();

    let received = timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::Message(message) = subscriber_rx.recv().await.unwrap()
                && message.topic == topic
            {
                return message;
            }
        }
    })
    .await
    .expect("published message was not delivered");
    assert_eq!(received.payload, b"hello");
    assert!(!received.retain);

    // The read-only subscriber profile cannot publish even though it is connected.
    let refused = subscriber
        .publish(PublishRequest {
            topic,
            payload: b"nope".to_vec(),
            qos: 0,
            retain: false,
            user_properties: Vec::new(),
        })
        .unwrap_err();
    assert!(refused.to_string().contains("disabled"));
    publisher.stop().await;
    subscriber.stop().await;
}

fn plain_profile(host: &str, role: &str, protocol: &str, id: &str) -> BrokerConfig {
    BrokerConfig {
        id: role.to_owned(),
        name: role.to_owned(),
        host: host.to_owned(),
        port: var("MATE_TEST_PLAIN_PORT").map_or(1883, |port| port.parse().unwrap()),
        client_id: format!("mate-it-{role}-{id}"),
        protocol: protocol.to_owned(),
        allow_publish: true,
        subscriptions: Vec::new(),
        ..BrokerConfig::default()
    }
}

async fn next_message_on(events: &mut mpsc::Receiver<BrokerEvent>, topic: &str) -> Vec<u8> {
    timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::Message(message) = events.recv().await.expect("broker task ended")
                && message.topic == topic
            {
                return message.payload;
            }
        }
    })
    .await
    .expect("message was not delivered")
}

fn request(topic: &str, payload: &[u8], retain: bool) -> PublishRequest {
    PublishRequest {
        topic: topic.to_owned(),
        payload: payload.to_vec(),
        qos: 1,
        retain,
        user_properties: Vec::new(),
    }
}

/// A subscriber of `subscriber_protocol` talks to a publisher of `publisher_protocol`.
async fn exchange(subscriber_protocol: &str, publisher_protocol: &str) {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let filter = format!("mate-it/{id}/#");
    let topic = format!("mate-it/{id}/data");
    publish_retained(&host, &topic, b"RETAINED").await;

    let mut subscriber_profile = plain_profile(&host, "subscriber", subscriber_protocol, &id);
    subscriber_profile.subscriptions = vec![SubscriptionConfig::new(filter, 1)];
    let (subscriber_tx, mut subscriber_rx) = mpsc::channel(64);
    let (subscriber, _) = spawn_broker(subscriber_profile, subscriber_tx).unwrap();
    wait_until_connected(&mut subscriber_rx).await;
    assert_eq!(
        next_message_on(&mut subscriber_rx, &topic).await,
        b"RETAINED"
    );

    let (publisher_tx, mut publisher_rx) = mpsc::channel(64);
    let (publisher, _) = spawn_broker(
        plain_profile(&host, "publisher", publisher_protocol, &id),
        publisher_tx,
    )
    .unwrap();
    wait_until_connected(&mut publisher_rx).await;
    publisher.publish(request(&topic, b"LIVE", false)).unwrap();
    assert_eq!(next_message_on(&mut subscriber_rx, &topic).await, b"LIVE");

    // Remove the retained message again so reruns stay clean.
    publisher.publish(request(&topic, b"", true)).unwrap();
    publisher.stop().await;
    subscriber.stop().await;
}

#[tokio::test]
async fn mqtt5_subscriber_and_publisher_exchange_messages() {
    exchange("mqtt5", "mqtt5").await;
}

#[tokio::test]
async fn mqtt311_and_mqtt5_clients_interoperate() {
    exchange("mqtt5", "mqtt311").await;
    exchange("mqtt311", "mqtt5").await;
}

async fn live_subscription_changes(protocol: &str) {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let wanted = format!("mate-it/{id}/wanted");
    let other = format!("mate-it/{id}/other");

    let (tx, mut rx) = mpsc::channel(64);
    let (runtime, _) = spawn_broker(plain_profile(&host, "live", protocol, &id), tx).unwrap();
    wait_until_connected(&mut rx).await;

    // Nothing is subscribed yet, so the first message must not arrive.
    let (publisher_tx, mut publisher_rx) = mpsc::channel(64);
    let (publisher, _) = spawn_broker(
        plain_profile(&host, "live-pub", protocol, &id),
        publisher_tx,
    )
    .unwrap();
    wait_until_connected(&mut publisher_rx).await;
    publisher
        .publish(request(&wanted, b"before", false))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    runtime.subscribe(&wanted, 1).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    publisher
        .publish(request(&wanted, b"after", false))
        .unwrap();
    assert_eq!(next_message_on(&mut rx, &wanted).await, b"after");

    runtime.unsubscribe(&wanted).unwrap();
    runtime.subscribe(&other, 0).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    publisher
        .publish(request(&wanted, b"ignored", false))
        .unwrap();
    publisher.publish(request(&other, b"seen", false)).unwrap();
    // Delivery to one client is ordered, so "ignored" would arrive before "seen".
    let mut topics_before_seen = Vec::new();
    timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::Message(message) = rx.recv().await.expect("broker task ended") {
                if message.payload == b"seen" {
                    return;
                }
                topics_before_seen.push(message.topic);
            }
        }
    })
    .await
    .expect("message was not delivered");
    assert!(topics_before_seen.is_empty(), "{topics_before_seen:?}");

    publisher.stop().await;
    runtime.stop().await;
}

#[tokio::test]
async fn live_subscription_changes_apply_to_a_running_mqtt311_connection() {
    live_subscription_changes("mqtt311").await;
}

#[tokio::test]
async fn live_subscription_changes_apply_to_a_running_mqtt5_connection() {
    live_subscription_changes("mqtt5").await;
}

#[tokio::test]
async fn mqtt5_properties_and_dup_flag_reach_the_subscriber() {
    use rumqttc::v5::{
        AsyncClient as Client5, MqttOptions as Options5,
        mqttbytes::{QoS as QoS5, v5::PublishProperties},
    };
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let topic = format!("mate-it/{id}/props");
    let mut subscriber_profile = plain_profile(&host, "props-sub", "mqtt5", &id);
    subscriber_profile.subscriptions = vec![SubscriptionConfig::new(topic.clone(), 1)];
    let (tx, mut rx) = mpsc::channel(64);
    let (subscriber, _) = spawn_broker(subscriber_profile, tx).unwrap();
    wait_until_connected(&mut rx).await;

    let port = var("MATE_TEST_PLAIN_PORT").map_or(1883, |port| port.parse().unwrap());
    let (client, mut event_loop) = Client5::new(
        Options5::new(format!("mate-it-props-pub-{id}"), &host, port),
        10,
    );
    tokio::spawn(async move { while event_loop.poll().await.is_ok() {} });
    let properties = PublishProperties {
        content_type: Some("application/json".to_owned()),
        user_properties: vec![("origin".to_owned(), "plc-7".to_owned())],
        message_expiry_interval: Some(60),
        response_topic: Some("replies/here".to_owned()),
        ..Default::default()
    };
    client
        .publish_with_properties(&topic, QoS5::AtLeastOnce, false, "{}", properties)
        .await
        .unwrap();

    let message = timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::Message(message) = rx.recv().await.unwrap()
                && message.topic == topic
            {
                return message;
            }
        }
    })
    .await
    .expect("message was not delivered");
    let received = message.properties.expect("MQTT 5 properties");
    assert_eq!(received.content_type.as_deref(), Some("application/json"));
    assert_eq!(received.response_topic.as_deref(), Some("replies/here"));
    assert!(received.message_expiry_secs.is_some_and(|secs| secs <= 60));
    assert_eq!(
        received.user_properties,
        vec![("origin".to_owned(), "plc-7".to_owned())]
    );
    assert!(!message.dup);
    subscriber.stop().await;
}

#[tokio::test]
async fn two_instances_of_one_profile_stay_connected() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let profile = plain_profile(&host, "twin", "mqtt311", &id);
    assert!(profile.unique_client_id);
    let (first_tx, mut first_rx) = mpsc::channel(64);
    let (first, _) = spawn_broker(profile.clone(), first_tx).unwrap();
    wait_until_connected(&mut first_rx).await;
    let (second_tx, mut second_rx) = mpsc::channel(64);
    let (second, _) = spawn_broker(profile, second_tx).unwrap();
    wait_until_connected(&mut second_rx).await;
    assert_ne!(first.client_id(), second.client_id());

    // A shared client ID would make the broker drop one session and both would start reconnecting.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for events in [&mut first_rx, &mut second_rx] {
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event,
                    BrokerEvent::State {
                        state: ConnectionState::Reconnecting,
                        ..
                    }
                ),
                "{event:?}"
            );
        }
    }
    first.stop().await;
    second.stop().await;
}

#[tokio::test]
async fn a_fixed_client_id_shared_by_two_instances_gets_one_session_dropped() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let mut profile = plain_profile(&host, "fixed", "mqtt311", &id);
    profile.unique_client_id = false;
    let (first_tx, mut first_rx) = mpsc::channel(64);
    let (first, _) = spawn_broker(profile.clone(), first_tx).unwrap();
    wait_until_connected(&mut first_rx).await;
    let (second_tx, mut second_rx) = mpsc::channel(64);
    let (second, _) = spawn_broker(profile, second_tx).unwrap();
    wait_until_connected(&mut second_rx).await;

    let dropped = timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::State {
                state: ConnectionState::Reconnecting,
                ..
            } = first_rx.recv().await.expect("broker task ended")
            {
                return;
            }
        }
    })
    .await;
    assert!(dropped.is_ok(), "the first session was not taken over");
    first.stop().await;
    second.stop().await;
}

#[tokio::test]
async fn excluded_topics_are_dropped_and_the_exclusion_can_change_live() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let seen = format!("mate-it/{id}/line/status");
    let hidden = format!("mate-it/{id}/hmi/message");
    let mut profile = plain_profile(&host, "excluding", "mqtt311", &id);
    profile.subscriptions = vec![
        SubscriptionConfig::new(format!("mate-it/{id}/#"), 0),
        SubscriptionConfig::exclusion(format!("mate-it/{id}/hmi/#")),
    ];
    let (tx, mut rx) = mpsc::channel(64);
    let (runtime, _) = spawn_broker(profile, tx).unwrap();
    wait_until_connected(&mut rx).await;
    let (publisher_tx, mut publisher_rx) = mpsc::channel(64);
    let (publisher, _) = spawn_broker(
        plain_profile(&host, "excluding-pub", "mqtt311", &id),
        publisher_tx,
    )
    .unwrap();
    wait_until_connected(&mut publisher_rx).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The broker delivers in order, so a dropped message would arrive before "seen".
    let mut before_seen = Vec::new();
    publisher
        .publish(request(&hidden, b"dropped", false))
        .unwrap();
    publisher.publish(request(&seen, b"seen", false)).unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if let BrokerEvent::Message(message) = rx.recv().await.expect("broker task ended") {
                if message.topic == seen {
                    return;
                }
                before_seen.push(message.topic);
            }
        }
    })
    .await
    .expect("the subscribed message was not delivered");
    assert!(before_seen.is_empty(), "{before_seen:?}");

    runtime.unexclude(&format!("mate-it/{id}/hmi/#"));
    publisher
        .publish(request(&hidden, b"now visible", false))
        .unwrap();
    assert_eq!(next_message_on(&mut rx, &hidden).await, b"now visible");

    runtime.exclude(&format!("mate-it/{id}/hmi/#")).unwrap();
    publisher
        .publish(request(&hidden, b"dropped again", false))
        .unwrap();
    publisher
        .publish(request(&seen, b"seen again", false))
        .unwrap();
    assert_eq!(next_message_on(&mut rx, &seen).await, b"seen again");
    assert!(runtime.exclude("a/#/b").is_err());

    publisher.stop().await;
    runtime.stop().await;
}

#[tokio::test]
async fn system_topics_arrive_only_when_the_profile_asks_for_them() {
    let Some(host) = var("MATE_TEST_BROKER_HOST") else {
        eprintln!("skipped: MATE_TEST_BROKER_HOST not set");
        return;
    };
    let id = unique();
    let mut profile = plain_profile(&host, "sys", "mqtt311", &id);
    profile.subscriptions = vec![SubscriptionConfig::new("#", 0)];
    profile.show_system_topics = true;
    let (tx, mut rx) = mpsc::channel(256);
    let (runtime, _) = spawn_broker(profile, tx).unwrap();
    wait_until_connected(&mut rx).await;
    // Mosquitto publishes its statistics every sys_interval seconds (default 10).
    let topic = timeout(Duration::from_secs(20), async {
        loop {
            if let BrokerEvent::Message(message) = rx.recv().await.expect("broker task ended")
                && message.topic.starts_with("$SYS/")
            {
                return message.topic;
            }
        }
    })
    .await
    .expect("no $SYS message arrived");
    assert!(topic.starts_with("$SYS/"));
    runtime.stop().await;
}
