use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};

use crate::config::{AppConfig, BrokerConfig, CURRENT_SCHEMA_VERSION, config_path};

#[derive(Debug, Clone)]
pub struct BrokerStore {
    root: PathBuf,
    config_file: PathBuf,
}

impl BrokerStore {
    pub fn from_config_path(path: &Path) -> Result<Self> {
        let root = path
            .parent()
            .context("configuration file path has no parent directory")?
            .to_path_buf();
        Ok(Self {
            root,
            config_file: path.to_path_buf(),
        })
    }

    pub fn default_store() -> Result<Self> {
        Self::from_config_path(&config_path()?)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("brokers")
    }

    pub fn load(&self) -> Result<AppConfig> {
        let mut app = AppConfig::load(Some(&self.config_file))?;
        ensure!(
            app.schema_version <= CURRENT_SCHEMA_VERSION,
            "configuration schema version {} is newer than this MATE build supports",
            app.schema_version
        );
        if !app.brokers.is_empty() {
            self.migrate_legacy(&mut app)?;
        }
        app.brokers = self.list_profiles()?;
        Ok(app)
    }

    pub fn list_profiles(&self) -> Result<Vec<BrokerConfig>> {
        let profiles_dir = self.profiles_dir();
        if !profiles_dir.exists() {
            return Ok(Vec::new());
        }
        ensure_real_directory(&profiles_dir)?;
        let mut profiles = Vec::new();
        for entry in fs::read_dir(&profiles_dir)
            .with_context(|| format!("could not read {}", profiles_dir.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                continue;
            }
            ensure!(
                file_type.is_dir() && !file_type.is_symlink(),
                "broker entries must be real directories: {}",
                entry.path().display()
            );
            let id = entry.file_name().to_string_lossy().into_owned();
            validate_broker_id(&id)?;
            let path = entry.path().join("broker.toml");
            let source = fs::read_to_string(&path)
                .with_context(|| format!("could not read broker profile {}", path.display()))?;
            let mut profile: BrokerConfig = toml::from_str(&source)
                .with_context(|| format!("invalid TOML in {}", path.display()))?;
            ensure!(
                profile.schema_version <= CURRENT_SCHEMA_VERSION,
                "broker '{}' schema version {} is newer than this MATE build supports",
                id,
                profile.schema_version
            );
            ensure!(
                profile.id.is_empty() || profile.id == id,
                "broker ID in {} does not match its directory",
                path.display()
            );
            profile.id = id;
            profile.broker_dir = Some(entry.path());
            profiles.push(profile);
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    pub fn get(&self, id: &str) -> Result<BrokerConfig> {
        validate_broker_id(id)?;
        self.list_profiles()?
            .into_iter()
            .find(|profile| profile.id == id)
            .with_context(|| format!("no broker profile with ID '{id}'"))
    }

    pub fn create(&self, mut profile: BrokerConfig) -> Result<BrokerConfig> {
        let existing = self.list_profiles()?;
        if profile.id.is_empty() {
            profile.id =
                unique_broker_id(&profile.name, existing.iter().map(|item| item.id.as_str()));
        }
        validate_broker_id(&profile.id)?;
        if profile.client_id.is_empty() || profile.client_id == "mate" {
            profile.client_id = format!("mate-{}", profile.id);
        }
        ensure!(
            !existing.iter().any(|item| item.id == profile.id),
            "broker ID '{}' already exists",
            profile.id
        );
        ensure!(
            !existing.iter().any(|item| item.name == profile.name),
            "broker name '{}' already exists",
            profile.name
        );
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    pub fn update(&self, id: &str, update: impl FnOnce(&mut BrokerConfig)) -> Result<BrokerConfig> {
        let mut profile = self.get(id)?;
        update(&mut profile);
        ensure!(
            profile.id == id,
            "broker IDs are immutable; create a new profile to change its ID"
        );
        ensure!(
            !profile.name.trim().is_empty(),
            "broker name must not be empty"
        );
        ensure!(
            !self
                .list_profiles()?
                .iter()
                .any(|item| item.id != id && item.name == profile.name),
            "broker name '{}' is already in use",
            profile.name
        );
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    /// Moves the profile to the folder that matches its current name: the ID follows the name.
    /// The default-broker setting and the saved UI state (favorites, publish history) go along;
    /// a stored password keeps working and is moved to the new reference. Returns the profile
    /// under its new ID, or unchanged when the name still gives the same ID.
    pub fn follow_name(&self, id: &str) -> Result<BrokerConfig> {
        let mut profile = self.get(id)?;
        let others: Vec<String> = self
            .list_profiles()?
            .into_iter()
            .filter(|item| item.id != id)
            .map(|item| item.id)
            .collect();
        let new_id = unique_broker_id(&profile.name, others.iter().map(String::as_str));
        if new_id == id {
            return Ok(profile);
        }
        validate_broker_id(&new_id)?;
        let from = self.profiles_dir().join(id);
        let to = self.profiles_dir().join(&new_id);
        ensure!(!to.exists(), "{} already exists", to.display());
        fs::rename(&from, &to)
            .with_context(|| format!("could not rename {} to {}", from.display(), to.display()))?;
        let old_reference = format!("keyring:{id}");
        let new_reference = format!("keyring:{new_id}");
        if profile.unique_client_id && profile.client_id == format!("mate-{id}") {
            profile.client_id = format!("mate-{new_id}");
        }
        let moved_password = profile.auth.username.is_some()
            && profile.auth.password_ref.as_deref() == Some(old_reference.as_str())
            && crate::security::move_broker_password(&old_reference, &new_reference).is_ok();
        if moved_password {
            profile.auth.password_ref = Some(new_reference);
        }
        profile.id = new_id.clone();
        profile.broker_dir = None;
        if let Err(error) = self.save_profile(&mut profile) {
            let _ = fs::rename(&to, &from);
            if moved_password {
                let _ = crate::security::move_broker_password(
                    &format!("keyring:{new_id}"),
                    &old_reference,
                );
            }
            return Err(error);
        }
        // The profile is consistent now; what refers to it by ID is repaired as far as possible.
        if let Ok(mut app) = self.load()
            && app.general.default_broker.as_deref() == Some(id)
        {
            app.general.default_broker = Some(new_id.clone());
            let _ = self.save_general(&app);
        }
        if let Ok(mut settings) = crate::ui_state::UiSettings::load(&self.root) {
            settings.rename_broker(id, &new_id);
            let _ = settings.save(&self.root);
        }
        Ok(profile)
    }

    pub fn export_profile(&self, id: &str) -> Result<BrokerConfig> {
        let mut profile = self.get(id)?;
        profile.tls.client_key = None;
        if profile
            .tls
            .ca_file
            .as_deref()
            .is_some_and(is_managed_cert_path)
        {
            profile.tls.ca_file = None;
        }
        if profile
            .tls
            .client_cert
            .as_deref()
            .is_some_and(is_managed_cert_path)
        {
            profile.tls.client_cert = None;
        }
        profile.broker_dir = None;
        Ok(profile)
    }

    pub fn import_profile(&self, mut profile: BrokerConfig) -> Result<BrokerConfig> {
        profile.id.clear();
        profile.schema_version = CURRENT_SCHEMA_VERSION;
        profile.broker_dir = None;
        if profile
            .tls
            .ca_file
            .as_deref()
            .is_some_and(is_managed_cert_path)
        {
            profile.tls.ca_file = None;
        }
        if profile
            .tls
            .client_cert
            .as_deref()
            .is_some_and(is_managed_cert_path)
        {
            profile.tls.client_cert = None;
        }
        if profile
            .tls
            .client_key
            .as_deref()
            .is_some_and(is_managed_cert_path)
        {
            profile.tls.client_key = None;
        }
        self.create(profile)
    }

    /// Copies a profile under a new ID and name. Certificates imported into the original's
    /// directory are not copied; external file references are kept.
    pub fn duplicate_profile(&self, id: &str) -> Result<BrokerConfig> {
        let mut copy = self.get(id)?;
        let names: Vec<String> = self
            .list_profiles()?
            .into_iter()
            .map(|profile| profile.name)
            .collect();
        copy.name = unique_copy_name(&copy.name, &names);
        // Two sessions with the same client ID would make the broker drop one of them.
        copy.client_id.clear();
        self.import_profile(copy)
    }

    /// Sets (or with `None` clears) the profile that is used when none is selected.
    pub fn set_default_broker(&self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            self.get(id)?;
        }
        let mut app = self.load()?;
        app.general.default_broker = id.map(str::to_owned);
        self.save_general(&app)
    }

    /// Adds a subscription, or updates the QoS of an existing one; new subscriptions start enabled.
    pub fn add_subscription(&self, id: &str, topic: &str, qos: u8) -> Result<BrokerConfig> {
        let topic = topic.trim();
        ensure!(
            crate::topics::valid_topic_filter(topic),
            "'{topic}' is not a valid MQTT topic filter"
        );
        ensure!(qos <= 2, "QoS must be 0, 1, or 2");
        let mut profile = self.get(id)?;
        ensure!(
            !profile
                .subscriptions
                .iter()
                .any(|entry| entry.exclude && entry.topic == topic),
            "'{topic}' is an exclusion already; remove it first"
        );
        match profile
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.topic == topic)
        {
            Some(existing) => existing.qos = qos,
            None => profile
                .subscriptions
                .push(crate::config::SubscriptionConfig::new(topic, qos)),
        }
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    /// Adds an exclusion: messages matching the filter are dropped; subscriptions are not changed.
    pub fn add_exclusion(&self, id: &str, filter: &str) -> Result<BrokerConfig> {
        let filter = filter.trim();
        ensure!(
            crate::topics::valid_topic_filter(filter),
            "'{filter}' is not a valid MQTT topic filter"
        );
        let mut profile = self.get(id)?;
        match profile
            .subscriptions
            .iter()
            .find(|entry| entry.topic == filter)
        {
            Some(entry) if !entry.exclude => {
                bail!("'{filter}' is a subscription already; remove it first")
            }
            Some(_) => return Ok(profile),
            None => {}
        }
        profile
            .subscriptions
            .push(crate::config::SubscriptionConfig::exclusion(filter));
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    pub fn remove_subscription(&self, id: &str, topic: &str) -> Result<BrokerConfig> {
        let mut profile = self.get(id)?;
        let before = profile.subscriptions.len();
        profile
            .subscriptions
            .retain(|subscription| subscription.topic != topic);
        ensure!(
            profile.subscriptions.len() < before,
            "broker '{id}' has no subscription '{topic}'"
        );
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    pub fn set_subscription_enabled(
        &self,
        id: &str,
        topic: &str,
        enabled: bool,
    ) -> Result<BrokerConfig> {
        let mut profile = self.get(id)?;
        let subscription = profile
            .subscriptions
            .iter_mut()
            .find(|subscription| subscription.topic == topic)
            .with_context(|| format!("broker '{id}' has no subscription '{topic}'"))?;
        subscription.enabled = enabled;
        self.save_profile(&mut profile)?;
        Ok(profile)
    }

    pub fn save_profile(&self, profile: &mut BrokerConfig) -> Result<()> {
        validate_broker_id(&profile.id)?;
        profile.schema_version = CURRENT_SCHEMA_VERSION;
        let brokers_dir = self.profiles_dir();
        create_real_directory(&brokers_dir)?;
        let profile_dir = brokers_dir.join(&profile.id);
        create_real_directory(&profile_dir)?;
        let destination = profile_dir.join("broker.toml");
        let encoded =
            toml::to_string_pretty(profile).context("could not serialize broker profile")?;
        write_atomic(&destination, encoded.as_bytes())?;
        profile.broker_dir = Some(profile_dir);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        validate_broker_id(id)?;
        ensure_real_directory(&self.profiles_dir())?;
        let profile_dir = self.profiles_dir().join(id);
        ensure_real_directory(&profile_dir)?;
        fs::remove_dir_all(&profile_dir).with_context(|| {
            format!(
                "could not remove broker directory {}",
                profile_dir.display()
            )
        })
    }

    pub fn resolve_profile_path(&self, profile: &BrokerConfig, path: &Path) -> Result<PathBuf> {
        resolve_profile_path(profile, path)
    }

    pub fn save_general(&self, app: &AppConfig) -> Result<()> {
        let mut general = app.clone();
        general.schema_version = CURRENT_SCHEMA_VERSION;
        general.brokers.clear();
        let destination = &self.config_file;
        fs::create_dir_all(&self.root).with_context(|| {
            format!("could not create config directory {}", self.root.display())
        })?;
        let encoded = toml::to_string_pretty(&general).context("could not serialize settings")?;
        write_atomic(destination, encoded.as_bytes())
    }

    fn migrate_legacy(&self, app: &mut AppConfig) -> Result<()> {
        let legacy = std::mem::take(&mut app.brokers);
        let mut ids = Vec::new();
        for mut profile in legacy {
            if profile.id.is_empty() {
                profile.id = unique_broker_id(&profile.name, ids.iter().map(String::as_str));
            }
            ids.push(profile.id.clone());
            self.save_profile(&mut profile)?;
        }
        if let Some(default) = app.general.default_broker.as_mut()
            && let Some(profile) = self
                .list_profiles()?
                .iter()
                .find(|profile| profile.name == *default)
        {
            *default = profile.id.clone();
        }
        self.save_general(app)
    }
}

fn unique_copy_name(name: &str, existing: &[String]) -> String {
    let first = format!("{name} copy");
    if !existing.contains(&first) {
        return first;
    }
    (2..)
        .map(|number| format!("{name} copy {number}"))
        .find(|candidate| !existing.contains(candidate))
        .expect("the suffix range is unbounded")
}

fn is_managed_cert_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(component)) if component == "certs")
}

pub fn resolve_profile_path(profile: &BrokerConfig, path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let base = profile
        .broker_dir
        .as_deref()
        .with_context(|| format!("broker '{}' has no directory context", profile.id))?;
    ensure!(
        path.components().all(|component| !matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )),
        "relative certificate paths may not escape the broker directory"
    );
    Ok(base.join(path))
}

pub fn validate_broker_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !id.starts_with('-')
            && !id.ends_with('-'),
        "broker ID must be 1-64 lowercase ASCII letters, digits, or internal hyphens"
    );
    Ok(())
}

pub fn unique_broker_id<'a>(name: &str, existing: impl Iterator<Item = &'a str>) -> String {
    let base: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() {
        "broker".to_owned()
    } else {
        base
    };
    let existing: Vec<_> = existing.collect();
    if !existing.contains(&base.as_str()) {
        return base;
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !existing.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("broker ID suffix space exhausted")
}

fn create_real_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "expected a real directory at {}",
                path.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("could not create directory {}", path.display()))?;
            ensure_real_directory(path)
        }
        Err(error) => Err(error).with_context(|| format!("could not inspect {}", path.display())),
    }
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("directory does not exist: {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "expected a real directory at {}",
        path.display()
    );
    Ok(())
}

pub fn write_atomic(destination: &Path, contents: &[u8]) -> Result<()> {
    let parent = destination
        .parent()
        .context("destination has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create temporary file in {}", parent.display()))?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .with_context(|| format!("could not atomically replace {}", destination.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, BrokerConfig};

    fn store(directory: &Path) -> BrokerStore {
        BrokerStore::from_config_path(&directory.join("config.toml")).unwrap()
    }

    #[test]
    fn the_folder_follows_the_name_and_what_refers_to_the_id_goes_along() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let original = store
            .create(BrokerConfig {
                name: "Production".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let copy = store.duplicate_profile(&original.id).unwrap();
        assert_eq!(copy.id, "production-copy");
        assert_eq!(copy.client_id, "mate-production-copy");
        store.set_default_broker(Some(&copy.id)).unwrap();
        let mut settings = crate::ui_state::UiSettings::default();
        settings.favorites.insert(
            copy.id.clone(),
            std::collections::BTreeSet::from(["a/b".to_owned()]),
        );
        settings.save(store.root()).unwrap();

        // The name did not change: nothing moves.
        assert_eq!(store.follow_name(&copy.id).unwrap().id, "production-copy");

        store
            .update(&copy.id, |profile| profile.name = "Staging".to_owned())
            .unwrap();
        let moved = store.follow_name(&copy.id).unwrap();
        assert_eq!(
            (moved.id.as_str(), moved.name.as_str()),
            ("staging", "Staging")
        );
        assert_eq!(
            moved.client_id, "mate-staging",
            "the generated client ID follows"
        );
        assert!(store.profiles_dir().join("staging/broker.toml").is_file());
        assert!(!store.profiles_dir().join("production-copy").exists());
        assert_eq!(store.get("staging").unwrap().id, "staging");
        assert!(store.get("production-copy").is_err());
        assert_eq!(
            store.load().unwrap().general.default_broker.as_deref(),
            Some("staging")
        );
        let saved = crate::ui_state::UiSettings::load(store.root()).unwrap();
        assert!(
            saved.favorites.contains_key("staging")
                && !saved.favorites.contains_key("production-copy")
        );

        // A name that gives an ID already in use gets a number; a fixed client ID stays as set.
        store
            .update("staging", |profile| {
                profile.name = "Production".to_owned();
                profile.client_id = "fixed".to_owned();
            })
            .unwrap_err();
        store
            .update("staging", |profile| {
                profile.name = "production!".to_owned();
                profile.client_id = "fixed".to_owned();
                profile.unique_client_id = false;
            })
            .unwrap();
        let numbered = store.follow_name("staging").unwrap();
        assert_eq!(numbered.id, "production-2");
        assert_eq!(numbered.client_id, "fixed");
    }

    #[test]
    fn create_load_and_remove_use_separate_broker_directories() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let created = store
            .create(BrokerConfig {
                name: "Production Line".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        assert_eq!(created.id, "production-line");
        assert!(
            directory
                .path()
                .join("brokers/production-line/broker.toml")
                .is_file()
        );
        let loaded = store.get("production-line").unwrap();
        assert_eq!(
            loaded.broker_dir.as_deref(),
            Some(directory.path().join("brokers/production-line").as_path())
        );
        store.remove(&created.id).unwrap();
        assert!(!directory.path().join("brokers/production-line").exists());
    }

    #[test]
    fn exclusions_are_stored_with_the_profile_and_kept_apart_from_subscriptions() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "Lab".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        store.add_subscription(&profile.id, "#", 0).unwrap();
        let updated = store.add_exclusion(&profile.id, " +/hmi/# ").unwrap();
        assert_eq!(updated.subscriptions.len(), 2);
        assert!(updated.subscriptions[1].exclude && updated.subscriptions[1].topic == "+/hmi/#");
        assert_eq!(
            store
                .add_exclusion(&profile.id, "+/hmi/#")
                .unwrap()
                .subscriptions
                .len(),
            2
        );

        let reloaded = store.get(&profile.id).unwrap();
        assert!(!reloaded.subscriptions[0].exclude && reloaded.subscriptions[1].exclude);
        let text = std::fs::read_to_string(
            directory
                .path()
                .join("brokers")
                .join(&profile.id)
                .join("broker.toml"),
        )
        .unwrap();
        assert_eq!(
            text.matches("exclude = true").count(),
            1,
            "only exclusions are marked:\n{text}"
        );

        assert!(
            store.add_exclusion(&profile.id, "#").is_err(),
            "a subscription is not an exclusion"
        );
        assert!(store.add_subscription(&profile.id, "+/hmi/#", 0).is_err());
        assert!(store.add_exclusion(&profile.id, "a/#/b").is_err());
        store
            .set_subscription_enabled(&profile.id, "+/hmi/#", false)
            .unwrap();
        assert!(!store.get(&profile.id).unwrap().subscriptions[1].enabled);
        store.remove_subscription(&profile.id, "+/hmi/#").unwrap();
        assert_eq!(store.get(&profile.id).unwrap().subscriptions.len(), 1);
    }

    #[test]
    fn subscriptions_can_be_added_updated_disabled_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "Lab".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();

        store.add_subscription(&profile.id, "plant/#", 1).unwrap();
        store
            .add_subscription(&profile.id, " other/+/status ", 0)
            .unwrap();
        let updated = store.add_subscription(&profile.id, "plant/#", 2).unwrap();
        assert_eq!(updated.subscriptions.len(), 2);
        assert_eq!(updated.subscriptions[0].qos, 2);
        assert_eq!(updated.subscriptions[1].topic, "other/+/status");
        assert!(
            updated
                .subscriptions
                .iter()
                .all(|subscription| subscription.enabled)
        );

        let disabled = store
            .set_subscription_enabled(&profile.id, "plant/#", false)
            .unwrap();
        assert!(!disabled.subscriptions[0].enabled);
        // Re-adding keeps the enabled state chosen by the user.
        let readded = store.add_subscription(&profile.id, "plant/#", 0).unwrap();
        assert!(!readded.subscriptions[0].enabled);
        assert!(!store.get(&profile.id).unwrap().subscriptions[0].enabled);

        assert!(store.add_subscription(&profile.id, "a/#/b", 0).is_err());
        assert!(store.add_subscription(&profile.id, "", 0).is_err());
        assert!(store.add_subscription(&profile.id, "ok", 3).is_err());
        assert!(
            store
                .set_subscription_enabled(&profile.id, "nope", true)
                .is_err()
        );
        assert!(store.remove_subscription(&profile.id, "nope").is_err());

        let removed = store.remove_subscription(&profile.id, "plant/#").unwrap();
        assert_eq!(removed.subscriptions.len(), 1);
    }

    #[test]
    fn profiles_written_before_the_enabled_flag_load_as_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "Old".to_owned(),
                subscriptions: Vec::new(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let file = directory
            .path()
            .join("brokers")
            .join(&profile.id)
            .join("broker.toml");
        let mut text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("subscriptions = []\n", "");
        text.push_str("\n[[subscriptions]]\ntopic = \"legacy/#\"\nqos = 1\n");
        std::fs::write(&file, text).unwrap();
        let loaded = store.get(&profile.id).unwrap();
        assert_eq!(loaded.subscriptions.len(), 1);
        assert!(loaded.subscriptions[0].enabled);
    }

    #[test]
    fn duplicate_creates_an_independent_profile_with_a_unique_name_and_client_id() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let original = store
            .create(BrokerConfig {
                name: "Production".to_owned(),
                host: "mqtt.example.test".to_owned(),
                tls: crate::config::TlsConfig {
                    enabled: true,
                    ca_file: Some(PathBuf::from("certs/ca.pem")),
                    client_cert: Some(PathBuf::from("/etc/mqtt/client.crt")),
                    client_key: Some(PathBuf::from("/etc/mqtt/client.key")),
                    ..crate::config::TlsConfig::default()
                },
                ..BrokerConfig::default()
            })
            .unwrap();

        let first = store.duplicate_profile(&original.id).unwrap();
        let second = store.duplicate_profile(&original.id).unwrap();
        assert_eq!(first.name, "Production copy");
        assert_eq!(second.name, "Production copy 2");
        assert_ne!(first.id, original.id);
        assert_eq!(first.client_id, format!("mate-{}", first.id));
        assert_eq!(first.host, "mqtt.example.test");
        assert_eq!(
            first.tls.ca_file, None,
            "imported certificates stay with the original"
        );
        assert_eq!(
            first.tls.client_cert.as_deref(),
            Some(Path::new("/etc/mqtt/client.crt"))
        );
        assert_eq!(
            first.tls.client_key.as_deref(),
            Some(Path::new("/etc/mqtt/client.key"))
        );
        assert_eq!(
            store.get(&original.id).unwrap().tls.ca_file.as_deref(),
            Some(Path::new("certs/ca.pem"))
        );
        assert_eq!(store.list_profiles().unwrap().len(), 3);
        assert!(store.duplicate_profile("missing").is_err());
    }

    #[test]
    fn default_broker_can_be_set_cleared_and_must_exist() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "Lab".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();

        store.set_default_broker(Some(&profile.id)).unwrap();
        assert_eq!(
            store.load().unwrap().general.default_broker.as_deref(),
            Some("lab")
        );
        assert!(store.set_default_broker(Some("missing")).is_err());
        assert_eq!(
            store.load().unwrap().general.default_broker.as_deref(),
            Some("lab")
        );
        store.set_default_broker(None).unwrap();
        assert_eq!(store.load().unwrap().general.default_broker, None);
        assert_eq!(
            store.list_profiles().unwrap().len(),
            1,
            "profiles are untouched"
        );
    }

    #[test]
    fn migration_moves_legacy_profiles_and_default_to_id() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let legacy = AppConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            general: crate::config::GeneralConfig {
                default_broker: Some("production".to_owned()),
                ..crate::config::GeneralConfig::default()
            },
            log_format: crate::config::LogFormatConfig::default(),
            brokers: vec![BrokerConfig {
                name: "production".to_owned(),
                ..BrokerConfig::default()
            }],
        };
        legacy
            .save_atomic(&directory.path().join("config.toml"))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.brokers[0].id, "production");
        assert_eq!(loaded.general.default_broker.as_deref(), Some("production"));
        let root = AppConfig::load(Some(&directory.path().join("config.toml"))).unwrap();
        assert!(root.brokers.is_empty());
    }

    #[test]
    fn migration_honors_explicit_config_filename() {
        let directory = tempfile::tempdir().unwrap();
        let config_file = directory.path().join("settings.toml");
        AppConfig {
            brokers: vec![BrokerConfig {
                name: "lab".to_owned(),
                ..BrokerConfig::default()
            }],
            ..AppConfig::default()
        }
        .save_atomic(&config_file)
        .unwrap();
        let store = BrokerStore::from_config_path(&config_file).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.brokers[0].id, "lab");
        assert!(config_file.exists());
        assert!(directory.path().join("brokers/lab/broker.toml").exists());
    }

    #[test]
    fn relative_paths_resolve_against_profile_directory() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "dev".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        assert_eq!(
            store
                .resolve_profile_path(&profile, Path::new("certs/ca.pem"))
                .unwrap(),
            directory.path().join("brokers/dev/certs/ca.pem")
        );
        assert!(
            store
                .resolve_profile_path(&profile, Path::new("../secret.pem"))
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_relative_certificate_paths_resolve_under_broker_directory() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "windows".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        assert_eq!(
            store
                .resolve_profile_path(&profile, Path::new(r"certs\ca.pem"))
                .unwrap(),
            directory
                .path()
                .join("brokers")
                .join(&profile.id)
                .join(r"certs\ca.pem")
        );
    }

    #[test]
    fn ids_are_validated_and_stable_across_display_name_changes() {
        assert!(validate_broker_id("prod-2").is_ok());
        assert!(validate_broker_id("../prod").is_err());
        assert_eq!(
            unique_broker_id("Production", ["production"].into_iter()),
            "production-2"
        );
    }

    #[test]
    fn profile_updates_cannot_change_stable_ids() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(directory.path());
        let profile = store
            .create(BrokerConfig {
                name: "production".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        assert!(
            store
                .update(&profile.id, |profile| profile.id = "renamed".to_owned())
                .is_err()
        );
        assert_eq!(store.get(&profile.id).unwrap().id, profile.id);
    }
}
