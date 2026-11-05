use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn current_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    pub general: GeneralConfig,
    #[serde(skip_serializing_if = "LogFormatConfig::is_default")]
    pub log_format: LogFormatConfig,
    pub brokers: Vec<BrokerConfig>,
}

/// How `.mqttlog` lines are written and read back. The defaults are the documented layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogFormatConfig {
    /// One line with the placeholders `{time}`, `{topic}`, `{properties}` and `{payload}`;
    /// `{{` and `}}` stand for braces. `{time}` and `{topic}` are required.
    pub line: String,
    /// strftime format of `{time}`.
    pub time: String,
    /// Write and name files by UTC instead of the local time.
    pub utc: bool,
    /// Regular expression to read lines back, with the named groups `time`, `topic` and optionally
    /// `properties` and `payload`. Empty: the built-in reader for the default layout.
    pub pattern: String,
}

impl Default for LogFormatConfig {
    fn default() -> Self {
        Self {
            line: "{time} | T0   I: {topic} {properties} {payload}".to_owned(),
            time: "%Y-%m-%d %H:%M:%S,%3f".to_owned(),
            utc: true,
            pattern: String::new(),
        }
    }
}

impl LogFormatConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub theme: String,
    pub refresh_rate: u16,
    pub default_broker: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_owned(),
            refresh_rate: 15,
            default_broker: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrokerConfig {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub host: String,
    pub port: u16,
    pub protocol: String,
    pub client_id: String,
    /// Appends a random suffix per connection so several MATE instances can use one profile.
    pub unique_client_id: bool,
    pub auto_connect: bool,
    /// Publishing is off unless a profile opts in, so a browser session cannot change broker data.
    pub allow_publish: bool,
    /// Also subscribe to the broker's `$SYS/#` topics, which `#` does not match.
    pub show_system_topics: bool,
    pub tls: TlsConfig,
    pub auth: AuthConfig,
    pub subscriptions: Vec<SubscriptionConfig>,
    #[serde(skip)]
    pub broker_dir: Option<PathBuf>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: String::new(),
            name: String::new(),
            description: String::new(),
            host: "localhost".to_owned(),
            port: 1883,
            protocol: "mqtt311".to_owned(),
            client_id: "mate".to_owned(),
            unique_client_id: true,
            auto_connect: false,
            allow_publish: false,
            show_system_topics: false,
            tls: TlsConfig::default(),
            auth: AuthConfig::default(),
            subscriptions: Vec::new(),
            broker_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    pub enabled: bool,
    pub ca_file: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    pub username: Option<String>,
    pub password_ref: Option<String>,
}

/// The broker's statistics topics; a wildcard in the first level never matches them.
pub const SYSTEM_TOPICS: &str = "$SYS/#";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionConfig {
    pub topic: String,
    #[serde(default)]
    pub qos: u8,
    /// Disabled subscriptions stay in the profile but are not sent to the broker.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Not a subscription: messages whose topic matches `topic` are dropped after they arrive.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude: bool,
}

fn default_enabled() -> bool {
    true
}

impl SubscriptionConfig {
    pub fn new(topic: impl Into<String>, qos: u8) -> Self {
        Self {
            topic: topic.into(),
            qos,
            enabled: true,
            exclude: false,
        }
    }

    /// An exclusion of the messages matching `filter`, whatever the subscriptions deliver.
    pub fn exclusion(filter: impl Into<String>) -> Self {
        Self {
            exclude: true,
            ..Self::new(filter, 0)
        }
    }
}

impl AppConfig {
    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let path = match explicit_path {
            Some(path) => path.to_path_buf(),
            None => config_path()?,
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = fs::read_to_string(&path)
            .with_context(|| format!("could not read configuration at {}", path.display()))?;
        toml::from_str(&source).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create configuration directory {}",
                parent.display()
            )
        })?;
        let encoded = toml::to_string_pretty(self).context("could not serialize configuration")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!("could not create temporary config in {}", parent.display())
        })?;
        temporary.write_all(encoded.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .with_context(|| format!("could not atomically replace {}", path.display()))?;
        Ok(())
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "mate")
        .context("could not determine the platform configuration directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_format_has_defaults_and_is_only_written_when_changed() {
        let config: AppConfig = toml::from_str("[log_format]\nutc = false\n").unwrap();
        assert!(!config.log_format.utc);
        assert_eq!(config.log_format.time, LogFormatConfig::default().time);
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(
            text.contains("[log_format]") && text.contains("utc = false"),
            "{text}"
        );
        let plain = toml::to_string_pretty(&AppConfig::default()).unwrap();
        assert!(!plain.contains("log_format"), "{plain}");
    }

    #[test]
    fn configuration_round_trips_without_secret_fields() {
        let config = AppConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            general: GeneralConfig::default(),
            log_format: LogFormatConfig::default(),
            brokers: vec![BrokerConfig {
                name: "line-a".to_owned(),
                auth: AuthConfig {
                    username: Some("mate".to_owned()),
                    password_ref: Some("keyring:line-a".to_owned()),
                },
                ..BrokerConfig::default()
            }],
        };
        let encoded = toml::to_string(&config).expect("serialize config");
        let decoded: AppConfig = toml::from_str(&encoded).expect("deserialize config");
        assert_eq!(decoded.brokers[0].name, "line-a");
        assert!(
            decoded.brokers[0]
                .auth
                .password_ref
                .as_deref()
                .unwrap()
                .starts_with("keyring:")
        );
        assert!(!encoded.contains("password ="));
    }

    #[test]
    fn absent_default_config_loads_as_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.toml");
        let config = AppConfig::load(Some(&path)).unwrap();
        assert!(config.brokers.is_empty());
    }

    #[test]
    fn atomic_save_round_trips_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("config.toml");
        let config = AppConfig {
            brokers: vec![BrokerConfig {
                name: "test".to_owned(),
                host: "broker.local".to_owned(),
                ..BrokerConfig::default()
            }],
            ..AppConfig::default()
        };
        config.save_atomic(&path).unwrap();
        let loaded = AppConfig::load(Some(&path)).unwrap();
        assert_eq!(loaded.brokers[0].host, "broker.local");
    }

    #[test]
    fn documented_example_configuration_parses() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/example.toml");
        let store = crate::broker::BrokerStore::from_config_path(&config_path).unwrap();
        let config = store.load().unwrap();
        assert_eq!(config.brokers.len(), 3);
        let tls_profile = config
            .brokers
            .iter()
            .find(|profile| profile.id == "mosquitto-tls")
            .unwrap();
        assert!(tls_profile.tls.enabled);
        assert_eq!(
            tls_profile.auth.password_ref.as_deref(),
            Some("keyring:mosquitto-tls")
        );
        let mtls_profile = config
            .brokers
            .iter()
            .find(|profile| profile.id == "mosquitto-mtls")
            .unwrap();
        assert!(mtls_profile.tls.client_cert.is_some());
    }

    #[test]
    fn older_broker_profiles_receive_the_current_schema_default() {
        let profile: BrokerConfig =
            toml::from_str("name = 'legacy'\nhost = 'localhost'\n").unwrap();
        assert_eq!(profile.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
