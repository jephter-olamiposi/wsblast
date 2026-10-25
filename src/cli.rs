//! Command-line argument parsing for the `wsblast` binary, plus the mode and
//! report format enums shared with the library API.

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// High-throughput, CI-friendly WebSocket load testing and SLO gating tool.
#[derive(Debug, Parser, Clone)]
#[command(
    name = "wsblast",
    author,
    version,
    about = "High-performance WebSocket load-testing CLI with percentile latency & CI gating",
    long_about = "wsblast stresses WebSocket targets, computes high-resolution latency percentiles \
                  (p50/p95/p99/p99.9), maps failure taxonomy, and gates CI pipelines via SLO thresholds.",
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Target WebSocket endpoint (ws:// or wss://)
    #[arg(
        value_name = "URL",
        help = "Target WebSocket URL (e.g., ws://127.0.0.1:9001/ws or wss://api.example.com/stream)"
    )]
    pub url: Option<String>,

    /// Target WebSocket endpoint flag (alternative to positional argument)
    #[arg(short = 'u', long = "url", value_name = "URL", conflicts_with = "url")]
    pub url_flag: Option<String>,

    /// Number of concurrent WebSocket connection workers
    #[arg(
        short = 'c',
        long = "connections",
        default_value = "50",
        value_name = "NUM"
    )]
    pub connections: usize,

    /// Connection ramp rate in workers per second (0 = spawn all connections immediately)
    #[arg(
        long = "ramp-rate",
        default_value = "0",
        value_name = "WORKERS_PER_SEC"
    )]
    pub ramp_rate: u64,

    /// Total test duration including the connection ramp (e.g., '10s', '30s', '2m', '500ms')
    #[arg(
        short = 'd',
        long = "duration",
        default_value = "10s",
        value_name = "DURATION"
    )]
    pub duration: String,

    /// Staged load profile instead of -c, -d and --ramp-rate: comma-separated
    /// DURATION:CONNECTIONS stages, each moving the connection count linearly
    /// from the previous target (starting at 0), e.g. '30s:100,2m:100,30s:0'
    #[arg(
        long = "stages",
        value_name = "STAGES",
        conflicts_with_all = ["connections", "duration", "ramp_rate"]
    )]
    pub stages: Option<String>,

    /// Maximum total messages to send across all workers (optional limit)
    #[arg(short = 'n', long = "requests", value_name = "NUM")]
    pub requests: Option<u64>,

    /// Message rate per connection per second (0 = unthrottled / maximum possible)
    #[arg(
        short = 'r',
        long = "rate",
        default_value = "0",
        value_name = "MSG_PER_SEC"
    )]
    pub rate: u64,

    /// Inline message payload to send (supports {{timestamp}}, {{worker_id}}, {{seq}})
    #[arg(short = 'p', long = "payload", value_name = "TEXT")]
    pub payload: Option<String>,

    /// Path to a file containing the message payload
    #[arg(long = "payload-file", value_name = "FILE", conflicts_with = "payload")]
    pub payload_file: Option<PathBuf>,

    /// Send payload as WebSocket binary frames instead of text frames
    #[arg(long = "binary", default_value_t = false)]
    pub binary: bool,

    /// Custom HTTP header to include in the WebSocket handshake (can be specified multiple times)
    #[arg(short = 'H', long = "header", value_name = "HEADER:VALUE")]
    pub headers: Vec<String>,

    /// Optional WebSocket subprotocol to request during handshake
    #[arg(long = "subprotocol", value_name = "NAME")]
    pub subprotocol: Option<String>,

    /// Load testing execution mode
    #[arg(long = "mode", value_enum, default_value_t = TestMode::Echo)]
    pub mode: TestMode,

    /// Connection handshake timeout (e.g., '5s', '2000ms')
    #[arg(
        long = "connect-timeout",
        default_value = "5s",
        value_name = "DURATION"
    )]
    pub connect_timeout: String,

    /// Per-message response timeout in echo mode (e.g., '5s', '1000ms')
    #[arg(
        long = "message-timeout",
        default_value = "5s",
        value_name = "DURATION"
    )]
    pub message_timeout: String,

    /// Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:POINTER (an RFC 6901 pointer such as json:/seq)
    ///
    /// first: the first text or binary frame received after a send is taken as
    /// the reply, even when it is an unrelated server push such as a broadcast or
    /// heartbeat, which then records a wrong RTT.
    ///
    /// exact: the reply must be byte-identical to the frame that was sent.
    ///
    /// json:POINTER: the reply is parsed as JSON and the value at the RFC 6901
    /// pointer (e.g. json:/seq) must equal this request's {{seq}}, as a number or
    /// a numeric string. The payload must contain {{seq}}.
    ///
    /// With exact and json:POINTER, other data frames are counted as unsolicited
    /// and the worker keeps waiting for its reply until --message-timeout.
    #[arg(long = "match", value_name = "MODE", default_value = "first")]
    pub reply_match: String,

    /// Periodic ping heartbeat interval (e.g., '10s', '0s' to disable)
    #[arg(long = "ping-interval", default_value = "0s", value_name = "DURATION")]
    pub ping_interval: String,

    /// SLO threshold: Maximum acceptable p50 round-trip latency (e.g., '10ms', '500us')
    #[arg(long = "max-p50", value_name = "DURATION")]
    pub max_p50: Option<String>,

    /// SLO threshold: Maximum acceptable p95 round-trip latency (e.g., '50ms', '20ms')
    #[arg(long = "max-p95", value_name = "DURATION")]
    pub max_p95: Option<String>,

    /// SLO threshold: Maximum acceptable p99 round-trip latency (e.g., '100ms', '50ms')
    #[arg(long = "max-p99", value_name = "DURATION")]
    pub max_p99: Option<String>,

    /// SLO threshold: Maximum acceptable p99.9 round-trip latency (e.g., '250ms')
    #[arg(long = "max-p999", value_name = "DURATION")]
    pub max_p999: Option<String>,

    /// SLO threshold: Maximum acceptable error rate fraction (e.g., '0.01' for 1%, '0.001' for 0.1%)
    #[arg(long = "max-error-rate", value_name = "FLOAT")]
    pub max_error_rate: Option<f64>,

    /// SLO threshold: Minimum required message throughput (messages/sec)
    #[arg(long = "min-throughput", value_name = "RPS")]
    pub min_throughput: Option<f64>,

    /// Authentication type (none, static, apikey)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-type", value_name = "TYPE", value_enum, default_value_t = AuthType::None)]
    pub auth_type: AuthType,

    /// Static token for auth (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-token", value_name = "TOKEN")]
    pub auth_token: Option<String>,

    /// Command whose trimmed stdout is the token or API key, run through the platform shell before the run (replaces --auth-token or --auth-apikey)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-token-cmd",
        value_name = "COMMAND",
        conflicts_with_all = ["auth_token", "auth_apikey"]
    )]
    pub auth_token_cmd: Option<String>,

    /// Time allowed for each run of --auth-token-cmd [default: 30s]
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-token-cmd-timeout",
        value_name = "DURATION",
        requires = "auth_token_cmd"
    )]
    pub auth_token_cmd_timeout: Option<String>,

    /// Re-run --auth-token-cmd every DURATION during the run; new handshakes use the latest token
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-token-refresh",
        value_name = "DURATION",
        requires = "auth_token_cmd"
    )]
    pub auth_token_refresh: Option<String>,

    /// Header name for static token (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-header",
        value_name = "HEADER",
        default_value = "Authorization"
    )]
    pub auth_header: String,

    /// API Key (used with --auth-type apikey)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-apikey", value_name = "KEY")]
    pub auth_apikey: Option<String>,

    /// API Key header name (used with --auth-type apikey)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-apikey-header",
        value_name = "HEADER",
        default_value = "X-API-Key"
    )]
    pub auth_apikey_header: String,

    /// GraphQL WS operation name
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-operation-name", value_name = "NAME")]
    pub graphql_ws_operation_name: Option<String>,

    /// GraphQL WS operation (query/mutation/subscription)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-operation", value_name = "OPERATION")]
    pub graphql_ws_operation: Option<String>,

    /// GraphQL WS variables (JSON)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-variables", value_name = "JSON")]
    pub graphql_ws_variables: Option<String>,

    /// GraphQL WS extensions (JSON)
    #[cfg(feature = "graphql-ws")]
    #[arg(long = "graphql-ws-extensions", value_name = "JSON")]
    pub graphql_ws_extensions: Option<String>,

    /// Output report format
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Compare two JSON reports for regression detection
    #[cfg(feature = "compare")]
    #[arg(long = "compare", value_name = "FILE", num_args = 2)]
    pub compare: Vec<PathBuf>,

    /// Regression threshold percentage (default: 10%)
    #[cfg(feature = "compare")]
    #[arg(
        long = "compare-threshold",
        value_name = "PERCENT",
        default_value_t = 10.0
    )]
    pub compare_threshold: f64,

    /// Fail on regression (exit code 2)
    #[cfg(feature = "compare")]
    #[arg(long = "fail-on-regression", default_value_t = false)]
    pub fail_on_regression: bool,

    /// Record every data frame and connection event to FILE as JSON lines (see METHODOLOGY.md)
    #[cfg(feature = "capture")]
    #[arg(long = "capture", value_name = "FILE")]
    pub capture: Option<PathBuf>,

    /// Maximum records --capture writes; later ones are dropped and counted (0 = unlimited)
    #[cfg(feature = "capture")]
    #[arg(
        long = "capture-limit",
        value_name = "N",
        default_value_t = crate::capture::DEFAULT_LIMIT,
        requires = "capture"
    )]
    pub capture_limit: u64,

    /// Write output report to the specified file path
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output_file: Option<PathBuf>,

    /// Launch interactive terminal dashboard (TUI)
    #[arg(long = "tui", default_value_t = false)]
    pub tui: bool,

    /// Suppress interactive progress bar (recommended for non-interactive CI/CD runners)
    #[arg(long = "no-progress", default_value_t = false)]
    pub no_progress: bool,

    /// End a connection's worker after a timeout, I/O error or server close instead of reconnecting with backoff
    #[arg(long = "no-reconnect", default_value_t = false)]
    pub no_reconnect: bool,

    /// Serve live metrics at http://ADDR/metrics in the Prometheus text format while the run lasts (e.g. '127.0.0.1:9464'; port 0 picks a free port)
    #[arg(long = "prometheus-listen", value_name = "ADDR")]
    pub prometheus_listen: Option<String>,

    /// Export live metrics over OTLP/HTTP with protobuf bodies to this base URL, e.g. 'http://localhost:4318' (needs the otel feature)
    #[arg(long = "otel-endpoint", value_name = "URL")]
    pub otel_endpoint: Option<String>,

    /// How often metrics are exported over OTLP; the final values are also exported when the run ends
    #[arg(
        long = "otel-interval",
        value_name = "DURATION",
        default_value = "10s",
        requires = "otel_endpoint"
    )]
    pub otel_interval: String,

    /// OpenTelemetry service.name resource attribute [default: $OTEL_SERVICE_NAME, else wsblast]
    #[arg(
        long = "otel-service-name",
        value_name = "NAME",
        requires = "otel_endpoint"
    )]
    pub otel_service_name: Option<String>,

    /// Path to scenario YAML file defining multi-phase test (conflicts with individual flags)
    #[arg(long = "scenario", value_name = "FILE", conflicts_with_all = ["connections", "duration", "rate", "payload", "payload_file"])]
    pub scenario: Option<PathBuf>,

    /// Run only a specific phase from the scenario by name
    #[arg(long = "phase", value_name = "NAME", requires = "scenario")]
    pub phase: Option<String>,

    /// Generate the load on these agents (started with `wsblast agent`) instead of locally; comma-separated HOST:PORT, ws://HOST:PORT or wss://HOST:PORT
    #[cfg(feature = "distributed")]
    #[arg(long = "agents", value_name = "HOST:PORT", value_delimiter = ',')]
    pub agents: Vec<String>,

    /// Token the agents were started with [default: $WSBLAST_AGENT_TOKEN]
    #[cfg(feature = "distributed")]
    #[arg(long = "agent-token", value_name = "TOKEN", requires = "agents")]
    pub agent_token: Option<String>,

    /// Time the agents get to become ready before the start, and to send their results after the end of the run
    #[cfg(feature = "distributed")]
    #[arg(
        long = "agent-timeout",
        value_name = "DURATION",
        default_value = "30s",
        requires = "agents"
    )]
    pub agent_timeout: String,

    /// PEM file of CA certificates to trust for wss:// agents, in addition to the platform's roots
    #[cfg(feature = "distributed")]
    #[arg(long = "agent-ca", value_name = "PEM", requires = "agents")]
    pub agent_ca: Option<PathBuf>,

    /// A subcommand instead of a load test.
    #[cfg(any(feature = "mock", feature = "distributed"))]
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands. `wsblast <URL>` without one runs a load test.
#[cfg(any(feature = "mock", feature = "distributed"))]
#[derive(Debug, Clone, clap::Subcommand)]
pub(crate) enum Command {
    /// Run a local WebSocket test server: echo with optional delay, jitter, stalls, rejections and closes, or replay of a capture file
    #[cfg(feature = "mock")]
    Mock(MockArgs),
    /// Run a load agent that generates load for a coordinator (`wsblast URL --agents ...`), one test at a time
    #[cfg(feature = "distributed")]
    Agent(AgentArgs),
}

/// Flags of `wsblast agent`.
#[cfg(feature = "distributed")]
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct AgentArgs {
    /// Address to accept coordinators on (host:port, e.g. '0.0.0.0:7700'; port 0 picks a free port)
    #[arg(long = "listen", value_name = "ADDR")]
    pub listen: String,

    /// Token coordinators must present, at least 16 characters; required, here or in WSBLAST_AGENT_TOKEN
    #[arg(
        long = "token",
        value_name = "TOKEN",
        env = "WSBLAST_AGENT_TOKEN",
        hide_env_values = true
    )]
    pub token: Option<String>,

    /// PEM certificate chain to serve the control channel over TLS (wss://)
    #[arg(long = "tls-cert", value_name = "PEM", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for --tls-cert
    #[arg(long = "tls-key", value_name = "PEM", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
}

/// Flags of `wsblast mock`.
#[cfg(feature = "mock")]
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct MockArgs {
    /// Address to listen on (host:port; port 0 picks a free port)
    #[arg(long = "listen", value_name = "ADDR", default_value = crate::mock::DEFAULT_LISTEN)]
    pub listen: String,

    /// Hold every reply for this long after its request arrived (e.g. '20ms')
    #[arg(long = "delay", value_name = "DURATION", default_value = "0s")]
    pub delay: String,

    /// Add a uniformly distributed extra delay between 0 and this to every reply
    #[arg(long = "jitter", value_name = "DURATION", default_value = "0s")]
    pub jitter: String,

    /// Close each connection with code 1000 after it sent N replies
    #[arg(long = "close-after", value_name = "N")]
    pub close_after: Option<u64>,

    /// Answer this fraction (0.0 to 1.0) of upgrade requests with HTTP 503
    #[arg(long = "reject-ratio", value_name = "RATIO", default_value_t = 0.0)]
    pub reject_ratio: f64,

    /// Stop replying on every connection at each multiple of this period (needs --stall-for)
    #[arg(long = "stall-every", value_name = "DURATION", requires = "stall_for")]
    pub stall_every: Option<String>,

    /// How long each stall lasts; shorter than --stall-every
    #[arg(long = "stall-for", value_name = "DURATION", requires = "stall_every")]
    pub stall_for: Option<String>,

    /// Instead of echoing, send each connection the frames a --capture file recorded as received, with their timing
    #[arg(long = "replay", value_name = "FILE")]
    pub replay: Option<PathBuf>,
}

impl Cli {
    /// Resolves target URL from either positional or named argument syntax.
    pub(crate) fn resolve_url(&self) -> Option<&str> {
        self.url.as_deref().or(self.url_flag.as_deref())
    }

    /// Returns true if running in scenario mode
    pub(crate) fn is_scenario_mode(&self) -> bool {
        self.scenario.is_some()
    }
}

/// Authentication method for WebSocket handshake.
#[cfg(feature = "auth")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AuthType {
    /// No authentication
    None,
    /// Static bearer token
    Static,
    /// API Key
    #[value(name = "apikey", alias = "api-key")]
    ApiKey,
}

/// What each worker does on its connection.
///
/// Only [`TestMode::Echo`] measures round-trip time, so latency SLOs require it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[non_exhaustive]
pub enum TestMode {
    /// Send request frame, await response, and record round-trip latency (RTT).
    Echo,
    /// Continuously stream frames without awaiting per-frame acknowledgments.
    Stream,
    /// Connect workers and passively record incoming server-pushed broadcast messages.
    Listen,
    /// Apollo GraphQL WS protocol (subscriptions).
    #[cfg_attr(docsrs, doc(cfg(feature = "graphql-ws")))]
    #[cfg(feature = "graphql-ws")]
    #[value(name = "graphql-ws", alias = "graph-qlws")]
    GraphQLWS,
}

/// Format of the report produced by [`render_report`](crate::render_report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputFormat {
    /// Human-readable styled terminal tables.
    Text,
    /// Schema-versioned JSON for CI parsing and metric archiving.
    Json,
    /// GitHub Flavored Markdown table for PR comments and job summaries.
    Markdown,
}
