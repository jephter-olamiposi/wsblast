//! Command-line argument parsing and flag specifications.

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
                  (p50/p95/p99/p99.9), maps failure taxonomy, and gates CI pipelines via SLO thresholds."
)]
pub struct Cli {
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

    /// Echo reply matching: first (default: the first data frame after a send is the reply), exact, or json:<POINTER>
    ///
    /// first: the first text or binary frame received after a send is taken as
    /// the reply, even when it is an unrelated server push such as a broadcast or
    /// heartbeat, which then records a wrong RTT.
    ///
    /// exact: the reply must be byte-identical to the frame that was sent.
    ///
    /// json:<POINTER>: the reply is parsed as JSON and the value at the RFC 6901
    /// pointer (e.g. json:/seq) must equal this request's {{seq}}, as a number or
    /// a numeric string. The payload must contain {{seq}}.
    ///
    /// With exact and json:<POINTER>, other data frames are counted as unsolicited
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

    /// Authentication type (static, jwt, oauth2, apikey)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-type", value_name = "TYPE", value_enum, default_value_t = AuthType::None)]
    pub auth_type: AuthType,

    /// Static token for auth (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-token", value_name = "TOKEN")]
    pub auth_token: Option<String>,

    /// Header name for static token (used with --auth-type static)
    #[cfg(feature = "auth")]
    #[arg(
        long = "auth-header",
        value_name = "HEADER",
        default_value = "Authorization"
    )]
    pub auth_header: String,

    /// JWT token endpoint (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-endpoint", value_name = "URL")]
    pub auth_jwt_endpoint: Option<String>,

    /// JWT client ID (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-client-id", value_name = "ID")]
    pub auth_jwt_client_id: Option<String>,

    /// JWT secret (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-secret", value_name = "SECRET")]
    pub auth_jwt_secret: Option<String>,

    /// JWT scopes (used with --auth-type jwt)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-jwt-scopes", value_name = "SCOPES", num_args = 1..)]
    pub auth_jwt_scopes: Vec<String>,

    /// OAuth2 token URL (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-token-url", value_name = "URL")]
    pub auth_oauth2_token_url: Option<String>,

    /// OAuth2 client ID (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-client-id", value_name = "ID")]
    pub auth_oauth2_client_id: Option<String>,

    /// OAuth2 client secret (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-client-secret", value_name = "SECRET")]
    pub auth_oauth2_client_secret: Option<String>,

    /// OAuth2 scopes (used with --auth-type oauth2)
    #[cfg(feature = "auth")]
    #[arg(long = "auth-oauth2-scopes", value_name = "SCOPES", num_args = 1..)]
    pub auth_oauth2_scopes: Vec<String>,

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

    /// Path to scenario YAML file defining multi-phase test (conflicts with individual flags)
    #[arg(long = "scenario", value_name = "FILE", conflicts_with_all = ["connections", "duration", "rate", "payload", "payload_file"])]
    pub scenario: Option<PathBuf>,

    /// Run only a specific phase from the scenario by name
    #[arg(long = "phase", value_name = "NAME", requires = "scenario")]
    pub phase: Option<String>,
}

impl Cli {
    /// Resolves target URL from either positional or named argument syntax.
    pub fn resolve_url(&self) -> Option<&str> {
        self.url.as_deref().or(self.url_flag.as_deref())
    }

    /// Returns true if running in scenario mode
    pub fn is_scenario_mode(&self) -> bool {
        self.scenario.is_some()
    }
}

/// Authentication method for WebSocket handshake.
#[cfg(feature = "auth")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthType {
    /// No authentication
    None,
    /// Static bearer token
    Static,
    /// JWT with client credentials
    Jwt,
    /// OAuth2 client credentials flow
    #[value(name = "oauth2", alias = "o-auth2")]
    OAuth2,
    /// API Key
    #[value(name = "apikey", alias = "api-key")]
    ApiKey,
}

/// Supported execution patterns for WebSocket load generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TestMode {
    /// Send request frame, await response, and record round-trip latency (RTT).
    Echo,
    /// Continuously stream frames without awaiting per-frame acknowledgments.
    Stream,
    /// Connect workers and passively record incoming server-pushed broadcast messages.
    Listen,
    /// Apollo GraphQL WS protocol (subscriptions).
    #[cfg(feature = "graphql-ws")]
    #[value(name = "graphql-ws", alias = "graph-qlws")]
    GraphQLWS,
}

/// Supported serialisation and presentation formats for test summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable styled terminal tables.
    Text,
    /// Schema-versioned JSON for CI parsing and metric archiving.
    Json,
    /// GitHub Flavored Markdown table for PR comments and job summaries.
    Markdown,
}
