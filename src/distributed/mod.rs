//! Distributed load generation: `wsblast agent` on every load machine and a
//! [`Coordinator`] (`wsblast URL --agents ...`) that splits a run between
//! them, starts them together and merges their results. A scenario runs
//! phase by phase, each phase split, started and merged the same way; frame
//! capture works across the agents too.
//!
//! The coordinator and the agents talk over a WebSocket control channel,
//! authenticated with a shared token sent as `Authorization: Bearer` and
//! optionally encrypted with TLS. The token and every handshake header of
//! the test, credentials included, travel over that channel, so use TLS or
//! a private network. docs/distributed.md in the repository describes the
//! protocol and the security model; METHODOLOGY.md, section 12, how the
//! results are merged.
//!
//! Run two agents and a coordinator in-process:
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use wsblast::distributed::{AgentConfig, AgentServer, Coordinator};
//! use wsblast::LoadTestConfig;
//!
//! # #[tokio::main]
//! # async fn main() -> wsblast::Result<()> {
//! let token = "a-long-random-shared-token";
//! let stop = CancellationToken::new();
//! let mut agents = Vec::new();
//! for _ in 0..2 {
//!     let config = AgentConfig::builder().listen("127.0.0.1:0").token(token).build()?;
//!     let agent = AgentServer::bind(config).await?;
//!     agents.push(agent.local_addr().to_string());
//!     tokio::spawn(agent.run(stop.clone()));
//! }
//!
//! let load = LoadTestConfig::builder("ws://127.0.0.1:9001")
//!     .connections(100)
//!     .duration(Duration::from_secs(10))
//!     .build()?;
//! let outcome = Coordinator::new(load, &agents, token)?
//!     .with_signal_handling(false)
//!     .run()
//!     .await?;
//! println!("p99 {:?} over {} agents", outcome.metrics.message_rtt.p99_duration(), agents.len());
//! stop.cancel();
//! # Ok(())
//! # }
//! ```

mod agent;
mod coordinator;
mod protocol;
mod tls;

pub use agent::{AgentConfig, AgentConfigBuilder, AgentServer};
#[cfg(feature = "scenario")]
pub use coordinator::DistributedScenarioOutcome;
pub use coordinator::{AgentOutcome, Coordinator, DEFAULT_AGENT_TIMEOUT, DistributedOutcome};
pub use protocol::{MIN_TOKEN_LEN, PROTOCOL_VERSION, TOKEN_ENV};
pub(crate) use protocol::{Share, WireRecord};

use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Time allowed for the TLS and WebSocket handshakes of a control
/// connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Time an agent waits for each setup message (`hello`, `prepare`).
const SETUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Control channel settings: results with long time series can be a few
/// megabytes, well within these limits.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(256 << 20))
        .max_frame_size(Some(256 << 20))
}

/// `wsblast agent`: validates the flags, serves until Ctrl+C or SIGTERM.
pub(crate) async fn run_agent_command(args: &crate::cli::AgentArgs) -> crate::Result<()> {
    let mut builder = AgentConfig::builder().listen(&args.listen);
    if let Some(token) = &args.token {
        builder = builder.token(token);
    }
    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        builder = builder.tls(cert, key);
    }
    let config = builder.build()?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let _signals = crate::runner::spawn_signal_listener(cancel.clone());
    let server = AgentServer::bind(config).await?;
    println!(
        "wsblast agent listening on {} (protocol {PROTOCOL_VERSION}, one test at a time)",
        server.url()
    );
    server.run(cancel).await?;
    println!("wsblast agent stopped");
    Ok(())
}
