//! TLS for the control channel: the agent's server configuration from PEM
//! files and the coordinator's client configuration.

use crate::error::{ConfigError, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;

fn read(path: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| {
        ConfigError::PayloadFile {
            path: format!("{} ({what})", path.display()),
            source,
        }
        .into()
    })
}

fn invalid(path: &Path, message: impl std::fmt::Display) -> crate::WsBlastError {
    ConfigError::InvalidOption(format!("{}: {message}", path.display())).into()
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let pem = read(path, "certificate")?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| invalid(path, format!("not a PEM certificate file: {e}")))?;
    if certs.is_empty() {
        return Err(invalid(path, "contains no PEM certificate"));
    }
    Ok(certs)
}

/// The agent's TLS server configuration: the certificate chain in
/// `cert_path` and the private key in `key_path`, both PEM.
pub(crate) fn server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>> {
    let certs = certificates(cert_path)?;
    let key_pem = read(key_path, "private key")?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| invalid(key_path, format!("no PEM private key found: {e}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| invalid(cert_path, e))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            invalid(
                cert_path,
                format!("certificate and key do not form a valid pair: {e}"),
            )
        })?;
    Ok(Arc::new(config))
}

/// The coordinator's TLS client configuration for `wss://` agents: the
/// platform's roots, as for `wss://` targets, plus the certificates in
/// `extra_ca` when given (for agents with a private CA or a self-signed
/// certificate).
pub(crate) fn client_config(extra_ca: Option<&Path>) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if let Some(path) = extra_ca {
        for cert in certificates(path)? {
            roots
                .add(cert)
                .map_err(|e| invalid(path, format!("not a usable CA certificate: {e}")))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ConfigError::InvalidOption(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}
