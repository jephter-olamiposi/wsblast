//! TLS for the control channel: the agent's server configuration from PEM
//! files (shared with `wsblast mock`) and the coordinator's client
//! configuration.

use crate::error::{ConfigError, Result};
use crate::tls::{certificates, invalid};
use std::path::Path;
use std::sync::Arc;

pub(crate) use crate::tls::server_config;

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
