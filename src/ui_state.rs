//! Per-user UI preferences (layout, favorites) kept separate from broker profiles.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::broker::write_atomic;

pub const FILE_NAME: &str = "ui-state.toml";

/// Messages published per broker that are offered again in the publish dialog.
pub const MAX_PUBLISH_HISTORY: usize = 20;
/// Larger payloads are sent but not remembered, to keep `ui-state.toml` small.
pub const MAX_REMEMBERED_PAYLOAD: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishHistoryEntry {
    pub topic: String,
    pub payload: String,
    pub qos: u8,
    pub retain: bool,
    /// Seconds since the Unix epoch.
    pub sent_at: u64,
}

/// Newest first; an identical earlier message moves to the front instead of being listed twice.
pub fn remember_publish(history: &mut Vec<PublishHistoryEntry>, entry: PublishHistoryEntry) {
    if entry.payload.len() > MAX_REMEMBERED_PAYLOAD {
        return;
    }
    history.retain(|known| {
        (&known.topic, &known.payload, known.qos, known.retain)
            != (&entry.topic, &entry.payload, entry.qos, entry.retain)
    });
    history.insert(0, entry);
    history.truncate(MAX_PUBLISH_HISTORY);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSettings {
    pub show_history: bool,
    pub topics_two_pane: u16,
    pub topics_three_pane: u16,
    pub history_three_pane: u16,
}

/// Copies the entries of `mine` that differ from `base` into `disk`, removals included.
fn merge_map<V: Clone + PartialEq>(
    base: &BTreeMap<String, V>,
    mine: &BTreeMap<String, V>,
    disk: &mut BTreeMap<String, V>,
) {
    for key in base.keys().chain(mine.keys()) {
        if base.get(key) == mine.get(key) {
            continue;
        }
        match mine.get(key) {
            Some(value) => disk.insert(key.clone(), value.clone()),
            None => disk.remove(key),
        };
    }
}

/// Where received messages are written as `.mqttlog` files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogSettings {
    pub enabled: bool,
    /// Directory of the log files; `~/` and relative paths (to the config folder) are accepted.
    pub directory: String,
    /// A file that would grow beyond this size is closed and the next block of the day begins.
    pub max_megabytes: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: String::new(),
            max_megabytes: crate::mqttlog::DEFAULT_MAX_MEGABYTES,
        }
    }
}

impl LogSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn max_bytes(&self) -> u64 {
        u64::from(self.max_megabytes.clamp(1, crate::mqttlog::MAX_MEGABYTES)) * 1024 * 1024
    }

    /// The configured directory, else `logs` inside the configuration folder.
    pub fn resolved_directory(&self, config_root: &Path) -> PathBuf {
        let wanted = self.directory.trim();
        if wanted.is_empty() {
            return config_root.join("logs");
        }
        if let Some(rest) = wanted.strip_prefix("~/")
            && let Some(dirs) = directories::BaseDirs::new()
        {
            return dirs.home_dir().join(rest);
        }
        let path = Path::new(wanted);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            config_root.join(path)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub layout: Option<LayoutSettings>,
    #[serde(skip_serializing_if = "LogSettings::is_default")]
    pub logging: LogSettings,
    /// Favorite topic paths per broker ID.
    pub favorites: BTreeMap<String, BTreeSet<String>>,
    /// Recently published messages per broker ID.
    pub publish_history: BTreeMap<String, Vec<PublishHistoryEntry>>,
}

impl UiSettings {
    /// A missing file yields the defaults; an unreadable one is an error the caller may ignore.
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join(FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{} is not valid; using defaults", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Carries what is saved per broker over to a new broker ID, also in the keys of broker sets.
    pub fn rename_broker(&mut self, old: &str, new: &str) {
        fn rename_keys<V>(map: &mut BTreeMap<String, V>, old: &str, new: &str) {
            let keys: Vec<String> = map
                .keys()
                .filter(|key| key.split('+').any(|part| part == old))
                .cloned()
                .collect();
            for key in keys {
                let mut parts: Vec<&str> = key
                    .split('+')
                    .map(|part| if part == old { new } else { part })
                    .collect();
                parts.sort_unstable();
                if let Some(value) = map.remove(&key) {
                    map.insert(parts.join("+"), value);
                }
            }
        }
        rename_keys(&mut self.favorites, old, new);
        rename_keys(&mut self.publish_history, old, new);
    }

    /// Applies what this instance changed (`mine` against its `base`) to what another instance may
    /// have saved meanwhile (`disk`); everything this instance did not touch stays as on disk.
    pub fn merge_changes(base: &Self, mine: &Self, mut disk: Self) -> Self {
        if mine.layout != base.layout {
            disk.layout = mine.layout.clone();
        }
        if mine.logging != base.logging {
            disk.logging = mine.logging.clone();
        }
        merge_map(&base.favorites, &mine.favorites, &mut disk.favorites);
        merge_map(
            &base.publish_history,
            &mine.publish_history,
            &mut disk.publish_history,
        );
        disk
    }

    pub fn save(&self, directory: &Path) -> Result<()> {
        fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let mut normalized = self.clone();
        normalized.favorites.retain(|_, topics| !topics.is_empty());
        normalized
            .publish_history
            .retain(|_, list| !list.is_empty());
        let encoded =
            toml::to_string_pretty(&normalized).context("could not serialize UI state")?;
        write_atomic(&directory.join(FILE_NAME), encoded.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults_and_settings_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            UiSettings::load(directory.path()).unwrap(),
            UiSettings::default()
        );

        let mut settings = UiSettings {
            layout: Some(LayoutSettings {
                show_history: true,
                topics_two_pane: 40,
                topics_three_pane: 20,
                history_three_pane: 35,
            }),
            ..UiSettings::default()
        };
        settings
            .favorites
            .insert("plant".to_owned(), BTreeSet::from(["a/b".to_owned()]));
        settings
            .favorites
            .insert("empty".to_owned(), BTreeSet::new());
        settings.save(directory.path()).unwrap();

        let loaded = UiSettings::load(directory.path()).unwrap();
        assert_eq!(loaded.layout, settings.layout);
        assert_eq!(
            loaded.favorites.len(),
            1,
            "brokers without favorites are not written"
        );
        assert!(loaded.favorites["plant"].contains("a/b"));
    }

    #[test]
    fn publish_history_is_newest_first_deduplicated_bounded_and_persisted() {
        let entry = |topic: &str, payload: &str| PublishHistoryEntry {
            topic: topic.to_owned(),
            payload: payload.to_owned(),
            qos: 1,
            retain: false,
            sent_at: 1,
        };
        let mut history = Vec::new();
        remember_publish(&mut history, entry("a", "1"));
        remember_publish(&mut history, entry("b", "2"));
        remember_publish(&mut history, entry("a", "1"));
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].topic, "a");

        for index in 0..MAX_PUBLISH_HISTORY + 5 {
            remember_publish(&mut history, entry("c", &index.to_string()));
        }
        assert_eq!(history.len(), MAX_PUBLISH_HISTORY);
        remember_publish(
            &mut history,
            entry("big", &"x".repeat(MAX_REMEMBERED_PAYLOAD + 1)),
        );
        assert!(history.iter().all(|known| known.topic != "big"));

        let directory = tempfile::tempdir().unwrap();
        let mut settings = UiSettings::default();
        settings
            .publish_history
            .insert("plant".to_owned(), history.clone());
        settings
            .publish_history
            .insert("empty".to_owned(), Vec::new());
        settings.save(directory.path()).unwrap();
        let loaded = UiSettings::load(directory.path()).unwrap();
        assert_eq!(loaded.publish_history.len(), 1);
        assert_eq!(loaded.publish_history["plant"], history);
    }

    #[test]
    fn renaming_a_broker_moves_its_saved_state_also_inside_broker_sets() {
        let mut settings = settings(
            None,
            &[
                ("old", &["a"]),
                ("old+zed", &["b"]),
                ("alpha+old", &["c"]),
                ("other", &["d"]),
            ],
            &[("old", "t")],
        );
        settings.rename_broker("old", "new");
        let keys: Vec<&str> = settings.favorites.keys().map(String::as_str).collect();
        assert_eq!(keys, ["alpha+new", "new", "new+zed", "other"]);
        assert!(settings.favorites["new+zed"].contains("b"));
        assert!(settings.publish_history.contains_key("new"));
        assert!(!settings.publish_history.contains_key("old"));
    }

    #[test]
    fn log_directories_resolve_against_the_config_folder() {
        let root = Path::new("/cfg");
        let mut logging = LogSettings::default();
        assert_eq!(logging.resolved_directory(root), Path::new("/cfg/logs"));
        logging.directory = "  captures ".to_owned();
        assert_eq!(logging.resolved_directory(root), Path::new("/cfg/captures"));
        logging.directory = "/var/log/mate".to_owned();
        assert_eq!(logging.resolved_directory(root), Path::new("/var/log/mate"));
    }

    #[test]
    fn logging_settings_round_trip_and_default_ones_are_not_written() {
        let directory = tempfile::tempdir().unwrap();
        UiSettings::default().save(directory.path()).unwrap();
        let text = fs::read_to_string(directory.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("logging"), "{text}");

        let settings = UiSettings {
            logging: LogSettings {
                enabled: true,
                directory: "/tmp/x".to_owned(),
                max_megabytes: 5,
            },
            ..UiSettings::default()
        };
        settings.save(directory.path()).unwrap();
        let loaded = UiSettings::load(directory.path()).unwrap();
        assert_eq!(loaded.logging, settings.logging);
    }

    #[test]
    fn unreadable_files_are_reported_instead_of_silently_replaced() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(FILE_NAME), "layout = [").unwrap();
        assert!(UiSettings::load(directory.path()).is_err());
    }

    #[test]
    fn unknown_or_partial_content_falls_back_per_field() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(FILE_NAME),
            "[favorites]\nlab = [\"x\"]\n",
        )
        .unwrap();
        let loaded = UiSettings::load(directory.path()).unwrap();
        assert!(loaded.layout.is_none());
        assert!(loaded.favorites["lab"].contains("x"));
    }

    fn settings(
        layout: Option<bool>,
        favorites: &[(&str, &[&str])],
        history: &[(&str, &str)],
    ) -> UiSettings {
        UiSettings {
            logging: LogSettings::default(),
            layout: layout.map(|show_history| LayoutSettings {
                show_history,
                topics_two_pane: 35,
                topics_three_pane: 25,
                history_three_pane: 30,
            }),
            favorites: favorites
                .iter()
                .map(|(broker, topics)| {
                    (
                        (*broker).to_owned(),
                        topics.iter().map(|topic| (*topic).to_owned()).collect(),
                    )
                })
                .collect(),
            publish_history: history
                .iter()
                .map(|(broker, topic)| {
                    (
                        (*broker).to_owned(),
                        vec![PublishHistoryEntry {
                            topic: (*topic).to_owned(),
                            payload: String::new(),
                            qos: 0,
                            retain: false,
                            sent_at: 0,
                        }],
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn merging_keeps_what_another_instance_saved_and_applies_only_own_changes() {
        let base = settings(None, &[("a", &["x"])], &[]);
        // This instance only added a favorite for broker b.
        let mine = settings(None, &[("a", &["x"]), ("b", &["y"])], &[]);
        // Another instance meanwhile changed the layout, favorites of a and the history of b.
        let disk = settings(Some(true), &[("a", &["x", "z"])], &[("b", "cmd")]);
        let merged = UiSettings::merge_changes(&base, &mine, disk);
        assert_eq!(
            merged.layout.as_ref().map(|layout| layout.show_history),
            Some(true)
        );
        assert_eq!(merged.favorites["a"].len(), 2, "untouched by this instance");
        assert!(
            merged.favorites["b"].contains("y"),
            "this instance's change"
        );
        assert_eq!(merged.publish_history["b"][0].topic, "cmd");
    }

    #[test]
    fn merging_applies_removals_and_lets_the_last_writer_win_for_a_key_it_changed() {
        let base = settings(Some(false), &[("a", &["x"]), ("b", &["y"])], &[]);
        let mine = settings(Some(true), &[("a", &["x2"])], &[]);
        let disk = settings(
            Some(false),
            &[("a", &["other"]), ("b", &["y"]), ("c", &["z"])],
            &[],
        );
        let merged = UiSettings::merge_changes(&base, &mine, disk);
        assert!(merged.layout.unwrap().show_history);
        assert!(merged.favorites["a"].contains("x2") && merged.favorites["a"].len() == 1);
        assert!(
            !merged.favorites.contains_key("b"),
            "this instance removed it"
        );
        assert!(
            merged.favorites.contains_key("c"),
            "another instance's broker survives"
        );
    }
}
