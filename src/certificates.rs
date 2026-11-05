use std::{
    fs::{self, File},
    io::{BufReader, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use x509_parser::prelude::parse_x509_certificate;

use crate::{
    broker::{BrokerStore, resolve_profile_path},
    config::BrokerConfig,
};

const MAX_CERTIFICATE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateKind {
    Ca,
    ClientCertificate,
    ClientPrivateKey,
}

impl CertificateKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "ca" => Ok(Self::Ca),
            "client-cert" => Ok(Self::ClientCertificate),
            "client-key" => Ok(Self::ClientPrivateKey),
            _ => anyhow::bail!("certificate kind must be ca, client-cert, or client-key"),
        }
    }

    pub fn filename(self) -> &'static str {
        match self {
            Self::Ca => "ca.pem",
            Self::ClientCertificate => "client.crt",
            Self::ClientPrivateKey => "client.key",
        }
    }

    fn path_slot(self, profile: &mut BrokerConfig) -> &mut Option<PathBuf> {
        match self {
            Self::Ca => &mut profile.tls.ca_file,
            Self::ClientCertificate => &mut profile.tls.client_cert,
            Self::ClientPrivateKey => &mut profile.tls.client_key,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CertificateMetadata {
    pub kind: CertificateKind,
    pub path: PathBuf,
    pub imported: bool,
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub not_before: Option<String>,
    pub not_after: Option<String>,
    pub expired: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct CertificateDetails {
    subject: Option<String>,
    issuer: Option<String>,
    not_before: Option<String>,
    not_after: Option<String>,
    expired: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CertificateService {
    store: BrokerStore,
}

impl CertificateService {
    pub fn new(store: BrokerStore) -> Self {
        Self { store }
    }

    pub fn import(
        &self,
        broker_id: &str,
        kind: CertificateKind,
        source: &Path,
        replace: bool,
    ) -> Result<PathBuf> {
        reject_symlink_components(source)?;
        let source_metadata = fs::symlink_metadata(source)
            .with_context(|| format!("certificate source does not exist: {}", source.display()))?;
        ensure!(
            source_metadata.is_file() && !source_metadata.file_type().is_symlink(),
            "certificate source must be a regular, non-symlink file"
        );
        ensure!(
            source_metadata.len() <= MAX_CERTIFICATE_BYTES,
            "certificate files may not exceed 4 MiB"
        );
        let mut input = File::open(source)
            .with_context(|| format!("could not open certificate source {}", source.display()))?;
        let mut bytes = Vec::with_capacity(source_metadata.len() as usize);
        input.read_to_end(&mut bytes)?;
        validate_bytes(kind, &bytes)?;

        let mut profile = self.store.get(broker_id)?;
        let profile_dir = profile
            .broker_dir
            .as_deref()
            .context("loaded broker profile has no directory")?;
        let certs_dir = profile_dir.join("certs");
        create_real_directory(&certs_dir)?;
        let destination = certs_dir.join(kind.filename());
        if destination.exists() && !replace {
            anyhow::bail!(
                "{} already exists; pass --replace to explicitly replace it",
                destination.display()
            );
        }
        if destination.exists() {
            let metadata = fs::symlink_metadata(&destination)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "refusing to replace a non-regular certificate destination"
            );
        }

        let mut temporary = tempfile::NamedTempFile::new_in(&certs_dir)
            .with_context(|| format!("could not stage certificate in {}", certs_dir.display()))?;
        if kind == CertificateKind::ClientPrivateKey {
            set_private_key_permissions(temporary.path())?;
        }
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&destination)
            .with_context(|| format!("could not atomically install {}", destination.display()))?;

        *kind.path_slot(&mut profile) = Some(PathBuf::from("certs").join(kind.filename()));
        profile.tls.enabled = true;
        self.store.save_profile(&mut profile)?;
        Ok(destination)
    }

    pub fn set_reference(&self, broker_id: &str, kind: CertificateKind, path: &Path) -> Result<()> {
        let mut profile = self.store.get(broker_id)?;
        let resolved = resolve_profile_path(&profile, path)?;
        validate_path(kind, &resolved)?;
        *kind.path_slot(&mut profile) = Some(path.to_path_buf());
        profile.tls.enabled = true;
        self.store.save_profile(&mut profile)
    }

    pub fn remove(&self, broker_id: &str, kind: CertificateKind) -> Result<()> {
        let mut profile = self.store.get(broker_id)?;
        let Some(configured_path) = kind.path_slot(&mut profile).take() else {
            anyhow::bail!(
                "no {} is configured for broker '{broker_id}'",
                kind.filename()
            );
        };
        let managed_path = if is_managed_path(&configured_path) {
            let path = resolve_profile_path(&profile, &configured_path)?;
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("imported certificate is missing: {}", path.display()))?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "refusing to remove a non-regular imported certificate"
            );
            Some(path)
        } else {
            None
        };
        self.store.save_profile(&mut profile)?;
        if let Some(path) = managed_path {
            fs::remove_file(&path).with_context(|| {
                format!("could not remove imported certificate {}", path.display())
            })?;
        }
        Ok(())
    }

    pub fn list(&self, broker_id: &str) -> Result<Vec<CertificateMetadata>> {
        let profile = self.store.get(broker_id)?;
        let mut entries = Vec::new();
        for (kind, configured) in [
            (CertificateKind::Ca, profile.tls.ca_file.as_ref()),
            (
                CertificateKind::ClientCertificate,
                profile.tls.client_cert.as_ref(),
            ),
            (
                CertificateKind::ClientPrivateKey,
                profile.tls.client_key.as_ref(),
            ),
        ] {
            if let Some(configured) = configured {
                let path = resolve_profile_path(&profile, configured)?;
                entries.push(inspect_path(kind, path, is_managed_path(configured)));
            }
        }
        Ok(entries)
    }

    pub fn validate_profile(&self, profile: &BrokerConfig) -> Result<()> {
        validate_tls_profile(profile)
    }

    pub fn export_bundle(
        &self,
        broker_id: &str,
        destination: &Path,
        include_private_key: bool,
        confirmed: bool,
    ) -> Result<()> {
        ensure!(
            confirmed,
            "certificate bundle export requires explicit --confirm-sensitive confirmation"
        );
        let mut profile = self.store.get(broker_id)?;
        let parent = destination
            .parent()
            .context("bundle destination has no parent directory")?;
        reject_symlink_components(parent)?;
        ensure!(
            !destination.exists(),
            "bundle destination already exists; refusing to overwrite"
        );
        let staged = tempfile::Builder::new()
            .prefix(".mate-certificate-export-")
            .tempdir_in(parent)
            .with_context(|| {
                format!("could not stage certificate bundle in {}", parent.display())
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o700))?;
        }
        let certs_dir = staged.path().join("certs");
        fs::create_dir(&certs_dir)?;

        for kind in [
            CertificateKind::Ca,
            CertificateKind::ClientCertificate,
            CertificateKind::ClientPrivateKey,
        ] {
            if kind == CertificateKind::ClientPrivateKey && !include_private_key {
                *kind.path_slot(&mut profile) = None;
                continue;
            }
            let Some(configured) = kind.path_slot(&mut profile).clone() else {
                continue;
            };
            let source = resolve_profile_path(&profile, &configured)?;
            reject_symlink_components(&source)?;
            validate_path(kind, &source)?;
            let bytes = fs::read(&source)
                .with_context(|| format!("could not read {}", source.display()))?;
            let target = certs_dir.join(kind.filename());
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("could not create {}", target.display()))?;
            if kind == CertificateKind::ClientPrivateKey {
                set_private_key_permissions(&target)?;
            }
            file.write_all(&bytes)?;
            file.sync_all()?;
            *kind.path_slot(&mut profile) = Some(PathBuf::from("certs").join(kind.filename()));
        }
        profile.broker_dir = None;
        let encoded = toml::to_string_pretty(&profile)
            .context("could not serialize exported broker profile")?;
        let mut config_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged.path().join("broker.toml"))?;
        config_file.write_all(encoded.as_bytes())?;
        config_file.sync_all()?;

        let staged_path = staged.keep();
        if let Err(error) = fs::rename(&staged_path, destination) {
            let _ = fs::remove_dir_all(&staged_path);
            return Err(error).with_context(|| {
                format!(
                    "could not atomically export bundle to {}",
                    destination.display()
                )
            });
        }
        Ok(())
    }

    pub fn import_bundle(&self, source_dir: &Path) -> Result<BrokerConfig> {
        reject_symlink_components(source_dir)?;
        let directory_metadata = fs::symlink_metadata(source_dir)?;
        ensure!(
            directory_metadata.is_dir() && !directory_metadata.file_type().is_symlink(),
            "certificate bundle must be a real directory"
        );
        let config_path = source_dir.join("broker.toml");
        let config_metadata = fs::symlink_metadata(&config_path)
            .with_context(|| format!("bundle profile is missing: {}", config_path.display()))?;
        ensure!(
            config_metadata.is_file() && !config_metadata.file_type().is_symlink(),
            "bundle broker.toml must be a regular file"
        );
        let source: BrokerConfig = toml::from_str(&fs::read_to_string(&config_path)?)
            .context("bundle broker.toml is invalid")?;

        let mut files = Vec::new();
        for (kind, configured) in [
            (CertificateKind::Ca, source.tls.ca_file.as_ref()),
            (
                CertificateKind::ClientCertificate,
                source.tls.client_cert.as_ref(),
            ),
            (
                CertificateKind::ClientPrivateKey,
                source.tls.client_key.as_ref(),
            ),
        ] {
            if let Some(configured) = configured {
                let expected = PathBuf::from("certs").join(kind.filename());
                ensure!(
                    configured.as_path() == expected.as_path(),
                    "bundle {} path must be certs/{}",
                    kind.filename(),
                    kind.filename()
                );
                let path = source_dir.join(configured);
                reject_symlink_components(&path)?;
                validate_path(kind, &path)
                    .with_context(|| format!("invalid bundle file {}", path.display()))?;
                files.push((kind, path));
            }
        }

        let mut imported = self.store.import_profile(source)?;
        for (kind, source_path) in files {
            if let Err(error) = self.import(&imported.id, kind, &source_path, false) {
                let _ = self.store.remove(&imported.id);
                return Err(error).context("could not import certificate bundle");
            }
        }
        imported = self.store.get(&imported.id)?;
        Ok(imported)
    }
}

pub fn validate_tls_profile(profile: &BrokerConfig) -> Result<()> {
    if !profile.tls.enabled {
        return Ok(());
    }
    for (kind, configured) in [
        (CertificateKind::Ca, profile.tls.ca_file.as_ref()),
        (
            CertificateKind::ClientCertificate,
            profile.tls.client_cert.as_ref(),
        ),
        (
            CertificateKind::ClientPrivateKey,
            profile.tls.client_key.as_ref(),
        ),
    ] {
        if let Some(configured) = configured {
            let path = resolve_profile_path(profile, configured)?;
            validate_path(kind, &path)
                .with_context(|| format!("invalid {} at {}", kind.filename(), path.display()))?;
        }
    }
    ensure!(
        profile.tls.client_cert.is_some() == profile.tls.client_key.is_some(),
        "mTLS requires both client certificate and client private key"
    );
    Ok(())
}

fn validate_path(kind: CertificateKind, path: &Path) -> Result<()> {
    // Follows symlinks on purpose: external references such as Let's Encrypt `live/` are links.
    let metadata = fs::metadata(path)
        .with_context(|| format!("configured certificate file is missing: {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "configured certificate must be a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_CERTIFICATE_BYTES,
        "certificate file exceeds 4 MiB"
    );
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    validate_bytes(kind, &bytes)
}

fn validate_bytes(kind: CertificateKind, bytes: &[u8]) -> Result<()> {
    ensure!(!bytes.is_empty(), "certificate or key file is empty");
    match kind {
        CertificateKind::ClientPrivateKey => {
            rustls_pemfile::private_key(&mut BufReader::new(Cursor::new(bytes)))
                .context("could not parse private key PEM")?
                .context("private key file contains no supported PEM key")?;
        }
        CertificateKind::Ca | CertificateKind::ClientCertificate => {
            let certificates = rustls_pemfile::certs(&mut Cursor::new(bytes))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("could not parse certificate PEM")?;
            ensure!(
                !certificates.is_empty(),
                "certificate PEM contains no certificates"
            );
            let now = x509_parser::time::ASN1Time::now();
            for der in certificates {
                let (remaining, certificate) = parse_x509_certificate(der.as_ref())
                    .map_err(|error| anyhow::anyhow!("invalid X.509 certificate: {error}"))?;
                ensure!(remaining.is_empty(), "X.509 certificate has trailing data");
                ensure!(
                    certificate.validity().not_before <= now,
                    "certificate is not valid yet"
                );
                ensure!(
                    now <= certificate.validity().not_after,
                    "certificate expired at {}",
                    certificate.validity().not_after
                );
                if kind == CertificateKind::Ca {
                    ensure!(
                        certificate.is_ca(),
                        "CA certificate is missing CA basic constraints"
                    );
                }
            }
        }
    }
    Ok(())
}

fn inspect_path(kind: CertificateKind, path: PathBuf, imported: bool) -> CertificateMetadata {
    if kind == CertificateKind::ClientPrivateKey {
        return match validate_path(kind, &path) {
            Ok(()) => metadata(kind, path, imported, CertificateDetails::default()),
            Err(error) => metadata(
                kind,
                path,
                imported,
                CertificateDetails {
                    error: Some(error.to_string()),
                    ..CertificateDetails::default()
                },
            ),
        };
    }
    match read_certificate_metadata(kind, &path) {
        Ok((subject, issuer, not_before, not_after, expired)) => metadata(
            kind,
            path,
            imported,
            CertificateDetails {
                subject: Some(subject),
                issuer: Some(issuer),
                not_before: Some(not_before),
                not_after: Some(not_after),
                expired,
                error: None,
            },
        ),
        Err(error) => metadata(
            kind,
            path,
            imported,
            CertificateDetails {
                error: Some(error.to_string()),
                ..CertificateDetails::default()
            },
        ),
    }
}

fn metadata(
    kind: CertificateKind,
    path: PathBuf,
    imported: bool,
    details: CertificateDetails,
) -> CertificateMetadata {
    CertificateMetadata {
        kind,
        path,
        imported,
        subject: details.subject,
        issuer: details.issuer,
        not_before: details.not_before,
        not_after: details.not_after,
        expired: details.expired,
        error: details.error,
    }
}

fn read_certificate_metadata(
    kind: CertificateKind,
    path: &Path,
) -> Result<(String, String, String, String, bool)> {
    let file_metadata = fs::metadata(path)
        .with_context(|| format!("configured certificate is missing: {}", path.display()))?;
    ensure!(
        file_metadata.is_file(),
        "configured certificate is not a regular file"
    );
    ensure!(
        file_metadata.len() <= MAX_CERTIFICATE_BYTES,
        "certificate file exceeds 4 MiB"
    );
    let bytes = fs::read(path)?;
    let der = rustls_pemfile::certs(&mut Cursor::new(bytes))
        .next()
        .context("certificate PEM contains no certificates")??;
    let (_, certificate) = parse_x509_certificate(der.as_ref())
        .map_err(|error| anyhow::anyhow!("invalid X.509 certificate: {error}"))?;
    if kind == CertificateKind::Ca {
        ensure!(
            certificate.is_ca(),
            "CA certificate is missing CA basic constraints"
        );
    }
    let validity = certificate.validity();
    let now = x509_parser::time::ASN1Time::now();
    Ok((
        certificate.subject().to_string(),
        certificate.issuer().to_string(),
        validity.not_before.to_string(),
        validity.not_after.to_string(),
        now > validity.not_after,
    ))
}

fn is_managed_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(value)) if value == "certs")
        && components.next().is_some()
        && components.next().is_none()
}

fn create_real_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "expected a real certificate directory at {}",
                path.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| {
                format!("could not create certificate directory {}", path.display())
            })?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("could not inspect {}", path.display())),
    }
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "certificate import source traverses a symlink: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
                "certificate import source does not exist: {}",
                current.display()
            ),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_key_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
fn set_private_key_permissions(path: &Path) -> Result<()> {
    use windows_acl::{
        acl::{ACL, AceType},
        helper::{current_user, name_to_sid, sid_to_string},
    };

    let path = path
        .to_str()
        .context("private key path is not valid UTF-8 for Windows ACL API")?;
    let username = current_user().context("could not determine current Windows user")?;
    let user_sid = name_to_sid(&username, None).map_err(|code| {
        anyhow::anyhow!("could not resolve current Windows user SID (error {code})")
    })?;
    let user_sid_string = sid_to_string(user_sid.as_ptr() as *mut _).map_err(|code| {
        anyhow::anyhow!("could not format current Windows user SID (error {code})")
    })?;
    let mut acl = ACL::from_file_path(path, false)
        .map_err(|code| anyhow::anyhow!("could not read private key ACL (error {code})"))?;
    for entry in acl
        .all()
        .map_err(|code| anyhow::anyhow!("could not inspect private key ACL (error {code})"))?
    {
        if entry.entry_type == AceType::AccessAllow
            && entry.string_sid != user_sid_string
            && entry.string_sid != "S-1-5-18"
            && entry.string_sid != "S-1-5-32-544"
        {
            if let Some(sid) = entry.sid {
                acl.remove_entry(
                    sid.as_ptr() as *mut _,
                    Some(entry.entry_type),
                    Some(entry.flags),
                )
                .map_err(|code| {
                    anyhow::anyhow!("could not restrict private key ACL (error {code})")
                })?;
            }
        }
    }
    acl.allow(user_sid.as_ptr() as *mut _, false, 0x001F01FF)
        .map_err(|code| {
            anyhow::anyhow!("could not grant current user access to private key (error {code})")
        })?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private_key_permissions(_path: &Path) -> Result<()> {
    anyhow::bail!("secure private-key imports are unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrokerConfig;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, date_time_ymd};

    fn generated_ca(expired: bool) -> (String, String) {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        if expired {
            params.not_before = date_time_ymd(1990, 1, 1);
            params.not_after = date_time_ymd(2000, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    #[test]
    fn missing_and_invalid_certificates_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store);
        assert!(service.validate_profile(&profile).is_ok());
        let source = directory.path().join("invalid.pem");
        fs::write(&source, b"not a certificate").unwrap();
        assert!(
            service
                .import(&profile.id, CertificateKind::Ca, &source, false)
                .is_err()
        );
        assert!(
            service
                .set_reference(
                    &profile.id,
                    CertificateKind::Ca,
                    Path::new("certs/missing.pem")
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn certificate_import_rejects_symlink_sources() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store);
        let (certificate, _) = generated_ca(false);
        let original = directory.path().join("source.pem");
        let alias = directory.path().join("alias.pem");
        fs::write(&original, certificate).unwrap();
        symlink(original, &alias).unwrap();
        assert!(
            service
                .import(&profile.id, CertificateKind::Ca, &alias, false)
                .is_err()
        );
        // An explicit external reference may be a symlink (e.g. Let's Encrypt `live/`).
        service
            .set_reference(&profile.id, CertificateKind::Ca, &alias)
            .unwrap();
        assert!(
            service
                .validate_profile(&store_profile(&service, &profile.id))
                .is_ok()
        );
    }

    #[cfg(unix)]
    fn store_profile(service: &CertificateService, id: &str) -> BrokerConfig {
        service.store.get(id).unwrap()
    }

    #[test]
    fn ca_import_preserves_source_and_requires_explicit_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store.clone());
        let source = directory.path().join("source.pem");
        let (certificate, _) = generated_ca(false);
        fs::write(&source, certificate.as_bytes()).unwrap();

        let imported = service
            .import(&profile.id, CertificateKind::Ca, &source, false)
            .unwrap();
        assert_eq!(fs::read(&source).unwrap(), fs::read(&imported).unwrap());
        assert!(
            service
                .import(&profile.id, CertificateKind::Ca, &source, false)
                .is_err()
        );
        assert!(
            service
                .import(&profile.id, CertificateKind::Ca, &source, true)
                .is_ok()
        );
        assert_eq!(
            store.get(&profile.id).unwrap().tls.ca_file,
            Some(PathBuf::from("certs/ca.pem"))
        );
        service.remove(&profile.id, CertificateKind::Ca).unwrap();
        assert!(!imported.exists());
        assert!(store.get(&profile.id).unwrap().tls.ca_file.is_none());
    }

    #[test]
    fn expired_certificate_metadata_is_visible_and_external_remove_preserves_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store);
        let source = directory.path().join("external-ca.pem");
        let (certificate, _) = generated_ca(true);
        fs::write(&source, certificate).unwrap();

        service
            .set_reference(&profile.id, CertificateKind::Ca, &source)
            .unwrap_err();
        let metadata = inspect_path(CertificateKind::Ca, source.clone(), false);
        assert!(metadata.expired);
        assert!(metadata.not_after.is_some());
        assert!(source.exists());

        let valid_external = directory.path().join("valid-external-ca.pem");
        let (certificate, _) = generated_ca(false);
        fs::write(&valid_external, certificate).unwrap();
        service
            .set_reference(&profile.id, CertificateKind::Ca, &valid_external)
            .unwrap();
        service.remove(&profile.id, CertificateKind::Ca).unwrap();
        assert!(valid_external.exists());
    }

    #[test]
    fn sensitive_bundle_export_requires_confirmation_and_standard_profiles_omit_key_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store.clone());
        let (certificate, key) = generated_ca(false);
        let cert_source = directory.path().join("ca.pem");
        let key_source = directory.path().join("client.key");
        fs::write(&cert_source, certificate).unwrap();
        fs::write(&key_source, &key).unwrap();
        service
            .import(&profile.id, CertificateKind::Ca, &cert_source, false)
            .unwrap();
        service
            .import(
                &profile.id,
                CertificateKind::ClientPrivateKey,
                &key_source,
                false,
            )
            .unwrap();

        let standard_export = store.export_profile(&profile.id).unwrap();
        assert!(standard_export.tls.client_key.is_none());
        let standard_toml = toml::to_string(&standard_export).unwrap();
        assert!(!standard_toml.contains("client_key"));

        let unconfirmed = directory.path().join("unconfirmed");
        assert!(
            service
                .export_bundle(&profile.id, &unconfirmed, true, false)
                .is_err()
        );
        assert!(!unconfirmed.exists());

        let profile_bundle = directory.path().join("profile-bundle");
        service
            .export_bundle(&profile.id, &profile_bundle, false, true)
            .unwrap();
        assert!(profile_bundle.join("certs/ca.pem").exists());
        assert!(!profile_bundle.join("certs/client.key").exists());
        let bundle_profile: BrokerConfig =
            toml::from_str(&fs::read_to_string(profile_bundle.join("broker.toml")).unwrap())
                .unwrap();
        assert!(bundle_profile.tls.client_key.is_none());

        let sensitive_bundle = directory.path().join("sensitive-bundle");
        service
            .export_bundle(&profile.id, &sensitive_bundle, true, true)
            .unwrap();
        let exported_key = sensitive_bundle.join("certs/client.key");
        assert_eq!(fs::read(&exported_key).unwrap(), key.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(exported_key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let bundle_config_path = sensitive_bundle.join("broker.toml");
        let mut bundle_profile: BrokerConfig =
            toml::from_str(&fs::read_to_string(&bundle_config_path).unwrap()).unwrap();
        bundle_profile.name = "restored".to_owned();
        fs::write(
            &bundle_config_path,
            toml::to_string(&bundle_profile).unwrap(),
        )
        .unwrap();
        let restored = service.import_bundle(&sensitive_bundle).unwrap();
        assert_eq!(restored.name, "restored");
        let restored_key = restored
            .broker_dir
            .as_deref()
            .unwrap()
            .join("certs/client.key");
        assert_eq!(fs::read(&restored_key).unwrap(), key.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(restored_key).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn imported_private_key_permissions_are_restricted() {
        let directory = tempfile::tempdir().unwrap();
        let store = BrokerStore::from_config_path(&directory.path().join("config.toml")).unwrap();
        let profile = store
            .create(BrokerConfig {
                name: "test".to_owned(),
                ..BrokerConfig::default()
            })
            .unwrap();
        let service = CertificateService::new(store);
        let source = directory.path().join("source.key");
        let (_, key) = generated_ca(false);
        fs::write(&source, &key).unwrap();
        let imported = service
            .import(
                &profile.id,
                CertificateKind::ClientPrivateKey,
                &source,
                false,
            )
            .unwrap();
        assert_eq!(fs::read(source).unwrap(), fs::read(&imported).unwrap());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(imported).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
