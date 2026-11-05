//! Metadata of the selected topic: message facts, history statistics and MQTT 5 properties. No drawing.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use mate::topics::Message;

use super::{
    DataSource, UiState,
    render::{format_size, thousands},
};

pub struct InfoSection {
    pub title: &'static str,
    pub rows: Vec<(&'static str, String)>,
}

fn timestamp(wall: SystemTime) -> String {
    DateTime::<Local>::from(wall)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

pub(super) fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{:.0} ms", seconds * 1000.0)
    } else if seconds < 120.0 {
        format!("{seconds:.1} s")
    } else if seconds < 7200.0 {
        format!("{:.1} min", seconds / 60.0)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_owned()
}

fn printable(data: &[u8]) -> String {
    match std::str::from_utf8(data) {
        Ok(text) if !text.chars().any(char::is_control) => format!("\"{text}\""),
        _ => data.iter().map(|byte| format!("{byte:02x}")).collect(),
    }
}

impl UiState {
    pub(super) fn info_sections(&self) -> Vec<InfoSection> {
        let Some(topic) = self.main.selected.as_deref() else {
            return Vec::new();
        };
        let mut sections = vec![InfoSection {
            title: "TOPIC",
            rows: vec![
                ("Topic", topic.to_owned()),
                ("Levels", topic.split('/').count().to_string()),
            ],
        }];
        let message = self.displayed_message();
        if let Some(message) = message {
            sections[0].rows.push((
                "Shown",
                if self.main.pinned.is_some() {
                    "historical message".to_owned()
                } else {
                    match self.data_source(message) {
                        DataSource::Live => "latest message, received live".to_owned(),
                        DataSource::Retained => "latest message, retained on the broker".to_owned(),
                        DataSource::Cached => "latest message, from the cache".to_owned(),
                    }
                },
            ));
            sections.push(self.message_section(message));
            // Properties come before the statistics so they stay on the first screen.
            sections.push(self.properties_section(message));
            sections.push(self.statistics_section(topic));
        } else {
            sections[0]
                .rows
                .push(("Kind", "group of topics, no message of its own".to_owned()));
        }

        let stats = self.topics.subtree_stats(topic);
        let own = usize::from(message.is_some());
        if stats.topics > own {
            let mut rows = vec![
                ("Topics below", thousands(stats.topics - own)),
                ("Messages received", thousands(stats.messages as usize)),
            ];
            if let Some(latest) = stats.latest {
                rows.push(("Latest message", timestamp(latest)));
            }
            sections.push(InfoSection {
                title: "SUBTREE",
                rows,
            });
        }
        sections
    }

    fn message_section(&self, message: &Message) -> InfoSection {
        let age = self.now.saturating_duration_since(message.received);
        let loaded = self.main.inspector.loaded();
        let kind = if message.payload.is_empty() {
            "empty".to_owned()
        } else if loaded.is_some_and(|loaded| loaded.doc.is_some()) {
            match message
                .payload
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
            {
                Some(b'{') => "JSON object",
                Some(b'[') => "JSON array",
                _ => "JSON value",
            }
            .to_owned()
        } else if std::str::from_utf8(&message.payload).is_ok() {
            "UTF-8 text".to_owned()
        } else {
            "binary (not valid UTF-8)".to_owned()
        };
        let previous = self.topics.previous_message(
            self.main.selected.as_deref().unwrap_or_default(),
            message.seq,
        );
        let change = match (previous, message.changed) {
            (None, _) => "first message in the history",
            (Some(_), true) => "differs from the previous message",
            (Some(_), false) => "same as the previous message",
        };
        let broker = message.broker.as_deref().and_then(|id| {
            self.broker_badge(id)
                .map(|slot| format!("{slot}  {}", self.broker_name(id)))
        });
        let mut rows = Vec::new();
        if let Some(broker) = broker {
            rows.push(("Broker", broker));
        }
        rows.extend([
            (
                "Received",
                format!("{} ({} ago)", timestamp(message.wall), duration_text(age)),
            ),
            (
                "Size",
                format!(
                    "{} ({} bytes)",
                    format_size(message.payload.len()),
                    thousands(message.payload.len())
                ),
            ),
            ("QoS", message.qos.to_string()),
            ("Retained", yes_no(message.retain)),
            ("Duplicate", yes_no(message.dup)),
            ("Payload", kind),
            ("Change", change.to_owned()),
        ]);
        InfoSection {
            title: "MESSAGE",
            rows,
        }
    }

    fn statistics_section(&self, topic: &str) -> InfoSection {
        let total = self.topics.received_total(topic);
        let history = self.topics.history(topic);
        let kept = history.map_or(0, |history| history.len());
        let mut rows = vec![
            (
                "Received",
                format!("{} since connecting", thousands(total as usize)),
            ),
            ("In history", thousands(kept)),
        ];
        if let Some(history) = history {
            if let Some(first) = history.front() {
                rows.push(("Oldest in history", timestamp(first.wall)));
            }
            let gaps: Vec<Duration> = history
                .iter()
                .zip(history.iter().skip(1))
                .map(|(earlier, later)| later.received.saturating_duration_since(earlier.received))
                .collect();
            if let (Some(min), Some(max)) = (gaps.iter().min(), gaps.iter().max()) {
                let average = gaps.iter().sum::<Duration>() / gaps.len() as u32;
                rows.push((
                    "Interval",
                    format!(
                        "avg {}, min {}, max {} ({} gaps)",
                        duration_text(average),
                        duration_text(*min),
                        duration_text(*max),
                        gaps.len()
                    ),
                ));
            }
            let changed = history.iter().filter(|entry| entry.changed).count();
            rows.push((
                "Changed payloads",
                format!("{changed} of {kept} in the history"),
            ));
        }
        InfoSection {
            title: "TOPIC STATISTICS",
            rows,
        }
    }

    fn properties_section(&self, message: &Message) -> InfoSection {
        let mut rows = Vec::new();
        match message.properties.as_deref() {
            Some(properties) if !properties.is_empty() => {
                if let Some(content_type) = &properties.content_type {
                    rows.push(("Content type", content_type.clone()));
                }
                if let Some(utf8) = properties.payload_is_utf8 {
                    rows.push((
                        "Payload format",
                        if utf8 {
                            "UTF-8 text"
                        } else {
                            "unspecified bytes"
                        }
                        .to_owned(),
                    ));
                }
                if let Some(seconds) = properties.message_expiry_secs {
                    rows.push(("Expiry", format!("{seconds} s")));
                }
                if let Some(response) = &properties.response_topic {
                    rows.push(("Response topic", response.clone()));
                }
                if let Some(data) = &properties.correlation_data {
                    rows.push(("Correlation data", printable(data)));
                }
                for (key, value) in &properties.user_properties {
                    rows.push(("User property", format!("{key} = {value}")));
                }
                if !properties.subscription_ids.is_empty() {
                    let ids: Vec<String> = properties
                        .subscription_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect();
                    rows.push(("Subscription IDs", ids.join(", ")));
                }
            }
            Some(_) => rows.push(("Properties", "none sent by the publisher".to_owned())),
            None => rows.push((
                "Properties",
                if self
                    .connections
                    .iter()
                    .any(|connection| connection.protocol == "MQTT 5")
                {
                    "none sent by the publisher".to_owned()
                } else {
                    "not available (MQTT 3.1.1 messages have no properties)".to_owned()
                },
            )),
        }
        InfoSection {
            title: "MQTT 5 PROPERTIES",
            rows,
        }
    }
}
