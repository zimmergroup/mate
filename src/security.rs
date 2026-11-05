use std::{
    fs,
    io::{BufReader, Cursor},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use rustls::{ClientConfig, RootCertStore};

use crate::{broker::resolve_profile_path, config::BrokerConfig};

pub fn broker_credentials(profile: &BrokerConfig) -> Result<Option<(String, String)>> {
    match (
        profile.auth.username.as_deref(),
        profile.auth.password_ref.as_deref(),
    ) {
        (None, None) => Ok(None),
        (Some(username), Some(reference)) => {
            ensure!(
                !username.trim().is_empty(),
                "broker username must not be empty"
            );
            let account = keyring_account(reference)?;
            let entry = keyring::Entry::new("mate", account)
                .context("could not open the operating-system credential store")?;
            let password = entry.get_password().context(
                "could not retrieve the broker password from the operating-system credential store",
            )?;
            Ok(Some((username.to_owned(), password)))
        }
        _ => anyhow::bail!("broker username and keyring password_ref must be configured together"),
    }
}

pub fn store_broker_password(profile: &BrokerConfig, password: &str) -> Result<()> {
    ensure!(!password.is_empty(), "password must not be empty");
    let reference = profile
        .auth
        .password_ref
        .as_deref()
        .context("broker has no password_ref; set one with --password-ref keyring:<account>")?;
    let account = keyring_account(reference)?;
    keyring::Entry::new("mate", account)
        .context("could not open the operating-system credential store")?
        .set_password(password)
        .context("could not store the password in the operating-system credential store")
}

/// Moves a stored password to another `keyring:<account>` reference; nothing happens when none is stored.
pub fn move_broker_password(old_reference: &str, new_reference: &str) -> Result<()> {
    let old = keyring::Entry::new("mate", keyring_account(old_reference)?)
        .context("could not open the operating-system credential store")?;
    let password = match old.get_password() {
        Ok(password) => password,
        Err(keyring::Error::NoEntry) => return Ok(()),
        Err(error) => return Err(error).context("could not read the stored broker password"),
    };
    keyring::Entry::new("mate", keyring_account(new_reference)?)
        .context("could not open the operating-system credential store")?
        .set_password(&password)
        .context("could not store the password under its new reference")?;
    old.delete_credential()
        .context("could not remove the password stored under the old reference")
}

fn keyring_account(reference: &str) -> Result<&str> {
    let account = reference.strip_prefix("keyring:").with_context(|| {
        "password_ref must use the keyring:<account> form; plaintext passwords are not supported"
    })?;
    ensure!(
        !account.trim().is_empty(),
        "keyring account reference must not be empty"
    );
    Ok(account)
}

pub fn tls_client_config(profile: &BrokerConfig) -> Result<Option<Arc<ClientConfig>>> {
    let tls = &profile.tls;
    if !tls.enabled {
        ensure!(
            tls.ca_file.is_none() && tls.client_cert.is_none() && tls.client_key.is_none(),
            "TLS certificate paths are configured while TLS is disabled"
        );
        return Ok(None);
    }
    if let Some(server_name) = tls.server_name.as_deref() {
        ensure!(
            server_name == profile.host,
            "rumqttc uses the broker hostname for both TCP and TLS verification; tls.server_name must match host"
        );
    }

    let mut roots = RootCertStore::empty();
    let system_roots = rustls_native_certs::load_native_certs();
    for error in system_roots.errors {
        tracing::warn!(%error, "some operating-system trust certificates could not be loaded");
    }
    roots.add_parsable_certificates(system_roots.certs);

    if let Some(ca_path) = tls.ca_file.as_deref() {
        let ca_path = resolve_profile_path(profile, ca_path)?;
        add_pem_certificates(&mut roots, &ca_path, "CA certificate")?;
    }
    ensure!(
        !roots.is_empty(),
        "no valid system or configured CA certificates are available"
    );

    let builder = ClientConfig::builder().with_root_certificates(roots);
    let config = match (&tls.client_cert, &tls.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_path = resolve_profile_path(profile, cert_path)?;
            let key_path = resolve_profile_path(profile, key_path)?;
            let certificates = read_certificates(&cert_path, "client certificate")?;
            let key_file = fs::File::open(&key_path).with_context(|| {
                format!("could not open client private key {}", key_path.display())
            })?;
            let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
                .context("could not parse client private key")?
                .context("client private key file contains no supported PEM private key")?;
            builder
                .with_client_auth_cert(certificates, key)
                .context("client certificate and private key are invalid or do not match")?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => anyhow::bail!("mTLS requires both client_cert and client_key paths"),
    };
    Ok(Some(Arc::new(config)))
}

fn add_pem_certificates(roots: &mut RootCertStore, path: &Path, label: &str) -> Result<()> {
    let certificates = read_certificates(path, label)?;
    let (added, _) = roots.add_parsable_certificates(certificates);
    ensure!(
        added > 0,
        "{} {} contains no valid certificates",
        label,
        path.display()
    );
    Ok(())
}

fn read_certificates(
    path: &Path,
    label: &str,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let bytes = fs::read(path)
        .with_context(|| format!("could not read {label} file {}", path.display()))?;
    let certificates = rustls_pemfile::certs(&mut Cursor::new(bytes))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("could not parse {label} PEM file {}", path.display()))?;
    ensure!(
        !certificates.is_empty(),
        "{} {} contains no certificates",
        label,
        path.display()
    );
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, BrokerConfig};

    #[test]
    fn credentials_require_a_keyring_reference_not_a_password() {
        let profile = BrokerConfig {
            auth: AuthConfig {
                username: Some("mate".to_owned()),
                password_ref: Some("plaintext-secret".to_owned()),
            },
            ..BrokerConfig::default()
        };
        assert!(broker_credentials(&profile).is_err());

        assert_eq!(keyring_account("keyring:production").unwrap(), "production");
        assert!(keyring_account("keyring:").is_err());
    }

    #[test]
    fn tls_fails_closed_for_incomplete_mtls_or_unsupported_server_name() {
        let mut profile = BrokerConfig::default();
        profile.tls.enabled = true;
        profile.tls.client_cert = Some("client.crt".into());
        assert!(tls_client_config(&profile).is_err());

        profile.tls.client_cert = None;
        profile.tls.server_name = Some("different.example".to_owned());
        assert!(tls_client_config(&profile).is_err());
    }
}
