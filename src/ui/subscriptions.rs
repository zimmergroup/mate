//! The subscription manager of one broker profile: list, enable/disable, add, remove, QoS, exclusions.
//! Changes are persisted through `BrokerStore` and, for the connected broker, applied live.

use crossterm::event::KeyCode;
use mate::config::{BrokerConfig, SubscriptionConfig};

use super::{UiAction, UiState, View, reload_profiles};

pub const FIELD_TOPIC: usize = 0;
pub const FIELD_QOS: usize = 1;

/// A change the event loop applies to the running connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionChange {
    Subscribe { topic: String, qos: u8 },
    Unsubscribe { topic: String },
    Exclude { topic: String },
    Unexclude { topic: String },
}

#[derive(Debug, Clone)]
pub struct SubscriptionEditor {
    pub topic: String,
    /// Adds an exclusion instead of a subscription.
    pub exclude: bool,
    pub qos: u8,
    pub field: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubscriptionsView {
    pub broker_id: String,
    pub selection: usize,
    pub editor: Option<SubscriptionEditor>,
}

impl UiState {
    /// Shown when a live subscribe/unsubscribe could not be queued on the connection.
    pub fn report_subscription_failure(&mut self, error: String) {
        self.show_notice(error, true);
    }
    pub(super) fn open_subscriptions(&mut self, broker_id: String) {
        self.subscriptions = Some(SubscriptionsView {
            broker_id,
            selection: 0,
            editor: None,
        });
        self.view = View::Subscriptions;
    }

    pub(super) fn subscriptions_profile(&self) -> Option<&BrokerConfig> {
        let view = self.subscriptions.as_ref()?;
        self.config
            .brokers
            .iter()
            .find(|profile| profile.id == view.broker_id)
    }

    pub(super) fn handle_subscriptions_key(&mut self, code: KeyCode) -> UiAction {
        if self
            .subscriptions
            .as_ref()
            .is_some_and(|view| view.editor.is_some())
        {
            return self.handle_subscription_editor_key(code);
        }
        let count = self
            .subscriptions_profile()
            .map_or(0, |profile| profile.subscriptions.len());
        let Some(view) = self.subscriptions.as_mut() else {
            self.view = View::Brokers;
            return UiAction::None;
        };
        match code {
            KeyCode::Esc | KeyCode::Char('s' | 'b') => {
                self.view = View::Brokers;
                self.subscriptions = None;
            }
            KeyCode::Char('q') => return UiAction::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                view.selection = (view.selection + 1).min(count.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => view.selection = view.selection.saturating_sub(1),
            KeyCode::Char(key @ ('a' | 'x')) => {
                view.editor = Some(SubscriptionEditor {
                    topic: String::new(),
                    exclude: key == 'x',
                    qos: 0,
                    field: FIELD_TOPIC,
                    error: None,
                });
            }
            KeyCode::Char(' ') | KeyCode::Enter => return self.toggle_selected_subscription(),
            KeyCode::Char(digit @ '0'..='2') => {
                return self.set_selected_subscription_qos(digit as u8 - b'0');
            }
            KeyCode::Char('d') | KeyCode::Delete => return self.remove_selected_subscription(),
            _ => {}
        }
        UiAction::None
    }

    fn selected_subscription(&self) -> Option<(String, SubscriptionConfig)> {
        let view = self.subscriptions.as_ref()?;
        let profile = self.subscriptions_profile()?;
        profile
            .subscriptions
            .get(view.selection)
            .map(|subscription| (profile.id.clone(), subscription.clone()))
    }

    /// The live change for the connected broker; nothing for a broker that is not running.
    fn live_change(&self, broker_id: &str, change: SubscriptionChange) -> UiAction {
        if self.has_connection(broker_id) {
            UiAction::Subscription(broker_id.to_owned(), change)
        } else {
            UiAction::None
        }
    }

    fn toggle_selected_subscription(&mut self) -> UiAction {
        let Some((id, subscription)) = self.selected_subscription() else {
            return UiAction::None;
        };
        let enabled = !subscription.enabled;
        if subscription.exclude {
            return self.toggle_exclusion(&id, &subscription.topic, enabled);
        }
        match self
            .broker_store
            .set_subscription_enabled(&id, &subscription.topic, enabled)
        {
            Ok(_) => {
                reload_profiles(self);
                let change = if enabled {
                    SubscriptionChange::Subscribe {
                        topic: subscription.topic.clone(),
                        qos: subscription.qos,
                    }
                } else {
                    self.notify(
                        "Unsubscribed; topics already received stay in the list",
                        false,
                    );
                    SubscriptionChange::Unsubscribe {
                        topic: subscription.topic.clone(),
                    }
                };
                self.live_change(&id, change)
            }
            Err(error) => {
                self.notify(format!("{error:#}"), true);
                UiAction::None
            }
        }
    }

    fn toggle_exclusion(&mut self, id: &str, topic: &str, enabled: bool) -> UiAction {
        match self
            .broker_store
            .set_subscription_enabled(id, topic, enabled)
        {
            Ok(_) => {
                reload_profiles(self);
                let topic = topic.to_owned();
                self.live_change(
                    id,
                    if enabled {
                        SubscriptionChange::Exclude { topic }
                    } else {
                        SubscriptionChange::Unexclude { topic }
                    },
                )
            }
            Err(error) => {
                self.notify(format!("{error:#}"), true);
                UiAction::None
            }
        }
    }

    fn set_selected_subscription_qos(&mut self, qos: u8) -> UiAction {
        let Some((id, subscription)) = self.selected_subscription() else {
            return UiAction::None;
        };
        if subscription.exclude || subscription.qos == qos {
            return UiAction::None;
        }
        match self
            .broker_store
            .add_subscription(&id, &subscription.topic, qos)
        {
            Ok(_) => {
                reload_profiles(self);
                if subscription.enabled {
                    self.live_change(
                        &id,
                        SubscriptionChange::Subscribe {
                            topic: subscription.topic,
                            qos,
                        },
                    )
                } else {
                    UiAction::None
                }
            }
            Err(error) => {
                self.notify(format!("{error:#}"), true);
                UiAction::None
            }
        }
    }

    fn remove_selected_subscription(&mut self) -> UiAction {
        let Some((id, subscription)) = self.selected_subscription() else {
            return UiAction::None;
        };
        match self
            .broker_store
            .remove_subscription(&id, &subscription.topic)
        {
            Ok(profile) => {
                reload_profiles(self);
                if let Some(view) = self.subscriptions.as_mut() {
                    view.selection = view
                        .selection
                        .min(profile.subscriptions.len().saturating_sub(1));
                }
                let topic = subscription.topic;
                if !subscription.enabled {
                    UiAction::None
                } else if subscription.exclude {
                    self.live_change(&id, SubscriptionChange::Unexclude { topic })
                } else {
                    self.live_change(&id, SubscriptionChange::Unsubscribe { topic })
                }
            }
            Err(error) => {
                self.notify(format!("{error:#}"), true);
                UiAction::None
            }
        }
    }

    fn handle_subscription_editor_key(&mut self, code: KeyCode) -> UiAction {
        let Some(view) = self.subscriptions.as_mut() else {
            return UiAction::None;
        };
        let Some(editor) = view.editor.as_mut() else {
            return UiAction::None;
        };
        editor.error = None;
        match code {
            KeyCode::Esc => view.editor = None,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down if !editor.exclude => {
                editor.field = (editor.field + 1) % 2;
            }
            KeyCode::Enter if editor.exclude => {
                let id = view.broker_id.clone();
                let topic = editor.topic.trim().to_owned();
                return match self.broker_store.add_exclusion(&id, &topic) {
                    Ok(profile) => {
                        reload_profiles(self);
                        if let Some(view) = self.subscriptions.as_mut() {
                            view.editor = None;
                            view.selection = profile
                                .subscriptions
                                .iter()
                                .position(|entry| entry.topic == topic)
                                .unwrap_or(0);
                        }
                        self.notify(
                            "Matching messages are dropped from now on; topics already received stay in the list",
                            false,
                        );
                        self.live_change(&id, SubscriptionChange::Exclude { topic })
                    }
                    Err(error) => {
                        if let Some(editor) = self
                            .subscriptions
                            .as_mut()
                            .and_then(|view| view.editor.as_mut())
                        {
                            editor.error = Some(format!("{error:#}"));
                        }
                        UiAction::None
                    }
                };
            }
            KeyCode::Enter if editor.field == FIELD_TOPIC => editor.field = FIELD_QOS,
            KeyCode::Enter => {
                let id = view.broker_id.clone();
                let topic = editor.topic.trim().to_owned();
                let qos = editor.qos;
                return match self.broker_store.add_subscription(&id, &topic, qos) {
                    Ok(profile) => {
                        reload_profiles(self);
                        if let Some(view) = self.subscriptions.as_mut() {
                            view.editor = None;
                            view.selection = profile
                                .subscriptions
                                .iter()
                                .position(|subscription| subscription.topic == topic)
                                .unwrap_or(0);
                        }
                        let enabled = profile
                            .subscriptions
                            .iter()
                            .find(|subscription| subscription.topic == topic)
                            .is_none_or(|subscription| subscription.enabled);
                        if enabled {
                            self.live_change(&id, SubscriptionChange::Subscribe { topic, qos })
                        } else {
                            UiAction::None
                        }
                    }
                    Err(error) => {
                        if let Some(editor) = self
                            .subscriptions
                            .as_mut()
                            .and_then(|view| view.editor.as_mut())
                        {
                            editor.error = Some(format!("{error:#}"));
                        }
                        UiAction::None
                    }
                };
            }
            KeyCode::Backspace if editor.field == FIELD_TOPIC => {
                editor.topic.pop();
            }
            KeyCode::Char(character) if editor.field == FIELD_TOPIC => editor.topic.push(character),
            KeyCode::Left if editor.field == FIELD_QOS => editor.qos = editor.qos.saturating_sub(1),
            KeyCode::Right if editor.field == FIELD_QOS => editor.qos = (editor.qos + 1).min(2),
            KeyCode::Char(digit @ '0'..='2') if editor.field == FIELD_QOS => {
                editor.qos = digit as u8 - b'0';
            }
            _ => {}
        }
        UiAction::None
    }
}
