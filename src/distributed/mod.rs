//! Distributed coordination module for multi-node WebSocket load testing.

use serde::{Deserialize, Serialize};
use url::Url;

pub use crate::cli::DistributedMode;

/// Configuration for distributed load testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedConfig {
    pub mode: DistributedMode,
    pub coordinator_url: Option<Url>,
    pub worker_id: Option<String>,
    pub worker_count: usize,
    pub bind_addr: String,
    pub coordinator_bind_addr: String,
}

impl Default for DistributedConfig {
    fn default() -> Self {
        Self {
            mode: DistributedMode::Worker,
            coordinator_url: None,
            worker_id: None,
            worker_count: 1,
            bind_addr: "127.0.0.1:50051".to_string(),
            coordinator_bind_addr: "127.0.0.1:50052".to_string(),
        }
    }
}

// tonic's generated service traits return `Status` by value; boxing it would break the
// generated trait signatures, so the large-error lint is suppressed for this module tree.
#[allow(clippy::result_large_err)]
#[cfg(feature = "distributed")]
pub mod coordinator;

#[allow(clippy::result_large_err)]
#[cfg(feature = "distributed")]
pub mod worker;

#[cfg(feature = "distributed")]
mod telemetry;

/// Installs a stderr tracing subscriber honouring `RUST_LOG` for distributed nodes.
///
/// A second call is a no-op, so coordinator and worker entry points can both
/// initialize without coordinating with the caller.
#[cfg(feature = "distributed")]
pub(crate) fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(feature = "distributed")]
pub use coordinator::run_coordinator;

#[cfg(feature = "distributed")]
pub use worker::run_worker;

// Re-export proto types - include generated code from source directory
#[allow(clippy::result_large_err)]
#[cfg(feature = "distributed")]
pub mod proto {
    pub mod wsblast {
        include!("proto/wsblast.rs");
    }
    pub use wsblast::*;
}
