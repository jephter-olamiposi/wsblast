//! Load test configuration: [`LoadTestConfig`] and its builder, the shared
//! validation, the conversion from command-line flags, and scenario files.

use crate::cli::{Cli, OutputFormat, TestMode};
use crate::error::{ConfigError, Result, WsBlastError};
use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
#[cfg(any(feature = "scenario", feature = "graphql-ws"))]
use serde::Deserialize;
#[cfg(feature = "graphql-ws")]
use serde::Serialize;
use std::fs;
#[cfg(feature = "scenario")]
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use url::Url;

/// Default text payload: a JSON template carrying a timestamp and sequence number.
const DEFAULT_TEXT_PAYLOAD: &str = r#"{"source":"wsblast","ts":"{{timestamp}}","seq":{{seq}}}"#;

/// Time allowed for each run of `--auth-token-cmd` by default.
#[cfg(feature = "auth")]
const DEFAULT_TOKEN_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Default binary payload used by `--binary` without `--payload`.
const DEFAULT_BINARY_PAYLOAD: &[u8] = b"wsblast-payload";

/// A validated load test configuration.
///
/// Build one with [`LoadTestConfig::builder`]. Every value has passed the same
/// validation the command line applies, so a [`Runner`](crate::Runner) never
/// sees an inconsistent configuration.
#[derive(Debug, Clone)]
pub struct LoadTestConfig {
    pub(crate) target_url: Url,
    pub(crate) connections: usize,
    pub(crate) ramp_rate: u64,
    pub(crate) duration: Duration,
    /// A staged load profile; empty for a constant connection count.
    pub(crate) stages: Vec<Stage>,
    pub(crate) max_requests: Option<u64>,
    pub(crate) rate_per_conn: u64,
    pub(crate) payload: PayloadConfig,
    pub(crate) headers: HeaderMap,
    pub(crate) subprotocol: Option<String>,
    pub(crate) mode: TestMode,
    pub(crate) connect_timeout: Duration,
    pub(crate) message_timeout: Duration,
    pub(crate) ping_interval: Duration,
    #[cfg(feature = "auth")]
    pub(crate) auth: Option<AuthConfig>,
    /// The authentication header of a run whose token is refreshed during
    /// the run; applied to every handshake attempt.
    #[cfg(feature = "auth")]
    pub(crate) live_auth: Option<crate::auth::LiveAuthHeader>,
    #[cfg(feature = "graphql-ws")]
    pub(crate) graphql_ws: Option<GraphQLWSConfig>,
    pub(crate) slo: SloThresholds,
    pub(crate) output_format: OutputFormat,
    pub(crate) output_path: Option<PathBuf>,
    pub(crate) tui: bool,
    pub(crate) no_progress: bool,
    /// Ends a worker on its first connection loss instead of reconnecting.
    pub(crate) no_reconnect: bool,
    /// Decides which inbound data frame answers an echo request.
    pub(crate) reply_match: ReplyMatch,
    /// Where `--capture` records frames, if anywhere.
    #[cfg(feature = "capture")]
    pub(crate) capture: Option<crate::capture::CaptureConfig>,
    /// On a distributed agent, which share of every stage target this
    /// machine opens; `stages` keeps the targets of the whole run.
    #[cfg(feature = "distributed")]
    pub(crate) stage_share: Option<crate::distributed::Share>,
}

impl LoadTestConfig {
    /// The number of connections a staged run should have open `elapsed`
    /// after it started: the stage target, or this agent's share of it.
    pub(crate) fn stage_target(&self, elapsed: Duration) -> usize {
        let target = Stage::target_at(&self.stages, elapsed);
        #[cfg(feature = "distributed")]
        if let Some(share) = self.stage_share {
            return share.of(target);
        }
        target
    }

    /// Starts a configuration for `target_url` with the command-line defaults.
    ///
    /// The URL is parsed and checked by
    /// [`build`](LoadTestConfigBuilder::build), together with every other
    /// setting.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use wsblast::{ConfigError, LoadTestConfig, SloThresholds, WsBlastError};
    ///
    /// # fn main() -> wsblast::Result<()> {
    /// let config = LoadTestConfig::builder("wss://api.example.com/stream")
    ///     .connections(200)
    ///     .ramp_rate(50)
    ///     .duration(Duration::from_secs(60))
    ///     .rate_per_connection(10)
    ///     .payload_text(r#"{"op":"ping","id":{{seq}}}"#)
    ///     .reply_match("json:/id".parse()?)
    ///     .header("Authorization", "Bearer token")
    ///     .slo(SloThresholds::new().with_max_p99(Duration::from_millis(50)))
    ///     .build()?;
    /// assert_eq!(config.connections(), 200);
    /// assert_eq!(config.target_url().host_str(), Some("api.example.com"));
    ///
    /// // Settings the command line rejects are rejected here too.
    /// let err = LoadTestConfig::builder("http://api.example.com").build().unwrap_err();
    /// assert!(matches!(err, WsBlastError::Config(ConfigError::UnsupportedScheme(_))));
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder(target_url: impl Into<String>) -> LoadTestConfigBuilder {
        LoadTestConfigBuilder::new(target_url.into())
    }

    /// WebSocket endpoint every worker connects to.
    pub fn target_url(&self) -> &Url {
        &self.target_url
    }

    /// Number of concurrent connections, one worker each.
    pub fn connections(&self) -> usize {
        self.connections
    }

    /// Workers started per second; `0` starts all of them at once.
    pub fn ramp_rate(&self) -> u64 {
        self.ramp_rate
    }

    /// Total run time, including the connection ramp.
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// The staged load profile, or an empty slice when the run holds a
    /// constant connection count. With stages, [`connections`](Self::connections)
    /// is the highest stage target and [`duration`](Self::duration) the sum of
    /// the stage durations.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Cap on messages sent across all workers, if any.
    pub fn max_requests(&self) -> Option<u64> {
        self.max_requests
    }

    /// Messages per second per connection; `0` means unthrottled.
    pub fn rate_per_connection(&self) -> u64 {
        self.rate_per_conn
    }

    /// Extra headers sent with every handshake.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// WebSocket subprotocol requested during the handshake, if any.
    pub fn subprotocol(&self) -> Option<&str> {
        self.subprotocol.as_deref()
    }

    /// What each worker does on its connection.
    pub fn mode(&self) -> TestMode {
        self.mode
    }

    /// Time allowed for the TCP, TLS and WebSocket handshake.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Time an echo request waits for its reply.
    pub fn message_timeout(&self) -> Duration {
        self.message_timeout
    }

    /// Keepalive ping interval; zero disables pings.
    pub fn ping_interval(&self) -> Duration {
        self.ping_interval
    }

    /// Rule that recognises the reply to an echo request.
    pub fn reply_match(&self) -> &ReplyMatch {
        &self.reply_match
    }

    /// SLO thresholds carried with the configuration.
    pub fn slo(&self) -> &SloThresholds {
        &self.slo
    }

    /// Format [`render_report`](crate::render_report) produces.
    pub fn output_format(&self) -> OutputFormat {
        self.output_format
    }

    /// Whether the runner draws a progress bar on stderr.
    pub fn progress(&self) -> bool {
        !self.no_progress && !self.tui
    }

    /// Whether workers reconnect after losing their connection.
    pub fn reconnect(&self) -> bool {
        !self.no_reconnect
    }

    /// File the run records its frames to, if any.
    #[cfg(feature = "capture")]
    #[cfg_attr(docsrs, doc(cfg(feature = "capture")))]
    pub fn capture_path(&self) -> Option<&std::path::Path> {
        self.capture.as_ref().map(|capture| capture.path.as_path())
    }
}

/// Builder for [`LoadTestConfig`], created by [`LoadTestConfig::builder`].
///
/// Unset values take the command-line defaults: 50 connections for 10 seconds
/// in echo mode, no ramp and no rate limit, a JSON text payload carrying
/// `{{seq}}` and `{{timestamp}}`, 5 second connect and message timeouts, no
/// keepalive pings, reconnects enabled and text output. Unlike the command
/// line, the builder disables the progress bar by default.
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct LoadTestConfigBuilder {
    target: String,
    connections: usize,
    ramp_rate: u64,
    duration: Duration,
    stages: Vec<Stage>,
    max_requests: Option<u64>,
    rate_per_conn: u64,
    payload: PayloadConfig,
    headers: HeaderMap,
    invalid_header: Option<String>,
    subprotocol: Option<String>,
    mode: TestMode,
    connect_timeout: Duration,
    message_timeout: Duration,
    ping_interval: Duration,
    #[cfg(feature = "graphql-ws")]
    graphql: GraphQLOptions,
    slo: SloThresholds,
    output_format: OutputFormat,
    progress: bool,
    reconnect: bool,
    reply_match: ReplyMatch,
    #[cfg(feature = "capture")]
    capture: Option<PathBuf>,
    #[cfg(feature = "capture")]
    capture_limit: u64,
}

/// GraphQL WS settings collected by the builder and checked in `build`.
#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone, Default)]
struct GraphQLOptions {
    operation: Option<String>,
    operation_name: Option<String>,
    variables: Option<serde_json::Value>,
    extensions: Option<serde_json::Value>,
}

impl LoadTestConfigBuilder {
    fn new(target: String) -> Self {
        Self {
            target,
            connections: 50,
            ramp_rate: 0,
            duration: Duration::from_secs(10),
            stages: Vec::new(),
            max_requests: None,
            rate_per_conn: 0,
            payload: PayloadConfig::from_text(DEFAULT_TEXT_PAYLOAD.to_string()),
            headers: HeaderMap::new(),
            invalid_header: None,
            subprotocol: None,
            mode: TestMode::Echo,
            connect_timeout: Duration::from_secs(5),
            message_timeout: Duration::from_secs(5),
            ping_interval: Duration::ZERO,
            #[cfg(feature = "graphql-ws")]
            graphql: GraphQLOptions::default(),
            slo: SloThresholds::default(),
            output_format: OutputFormat::Text,
            progress: false,
            reconnect: true,
            reply_match: ReplyMatch::First,
            #[cfg(feature = "capture")]
            capture: None,
            #[cfg(feature = "capture")]
            capture_limit: crate::capture::DEFAULT_LIMIT,
        }
    }

    /// Sets the number of concurrent connections. Must be greater than zero.
    pub fn connections(mut self, connections: usize) -> Self {
        self.connections = connections;
        self
    }

    /// Starts `workers_per_sec` workers per second instead of all at once.
    pub fn ramp_rate(mut self, workers_per_sec: u64) -> Self {
        self.ramp_rate = workers_per_sec;
        self
    }

    /// Sets the total run time, including the connection ramp.
    pub fn duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Runs a staged load profile instead of a constant connection count.
    ///
    /// Each stage moves the number of open connections linearly from the
    /// previous stage's target (0 for the first stage) to its own target over
    /// its duration. When the count falls, the most recently opened
    /// connections are closed first. The run lasts the sum of the stage
    /// durations and opens at most the highest target at once, which
    /// replace [`duration`](Self::duration) and
    /// [`connections`](Self::connections). Stages cannot be combined with
    /// [`ramp_rate`](Self::ramp_rate).
    ///
    /// ```
    /// use std::time::Duration;
    /// use wsblast::{LoadTestConfig, Stage};
    ///
    /// let config = LoadTestConfig::builder("ws://127.0.0.1:9001")
    ///     .stages(vec![
    ///         Stage::new(Duration::from_secs(30), 100),
    ///         Stage::new(Duration::from_secs(120), 100),
    ///         Stage::new(Duration::from_secs(30), 0),
    ///     ])
    ///     .build()?;
    /// assert_eq!(config.connections(), 100);
    /// assert_eq!(config.duration(), Duration::from_secs(180));
    /// # Ok::<(), wsblast::WsBlastError>(())
    /// ```
    pub fn stages(mut self, stages: Vec<Stage>) -> Self {
        self.stages = stages;
        self
    }

    /// Ends the run once `max_requests` messages were sent across all
    /// workers. Must be greater than zero.
    pub fn max_requests(mut self, max_requests: u64) -> Self {
        self.max_requests = Some(max_requests);
        self
    }

    /// Paces each connection to `messages_per_sec`; `0` means unthrottled.
    pub fn rate_per_connection(mut self, messages_per_sec: u64) -> Self {
        self.rate_per_conn = messages_per_sec;
        self
    }

    /// Sends `text` as text frames. The placeholders `{{seq}}`,
    /// `{{timestamp}}` and `{{worker_id}}` are filled in for every frame.
    pub fn payload_text(mut self, text: impl Into<String>) -> Self {
        self.payload = PayloadConfig::from_text(text.into());
        self
    }

    /// Sends `bytes` as binary frames, unchanged.
    pub fn payload_binary(mut self, bytes: impl Into<Bytes>) -> Self {
        self.payload = PayloadConfig::Binary(bytes.into());
        self
    }

    /// Adds a handshake header. Repeating a name sends every value.
    ///
    /// An invalid name or value is reported by [`build`](Self::build).
    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (
            HeaderName::from_str(name.trim()),
            HeaderValue::from_str(value.trim()),
        ) {
            (Ok(name), Ok(value)) => {
                self.headers.append(name, value);
            }
            (Err(_), _) => {
                self.invalid_header
                    .get_or_insert_with(|| format!("Invalid header name '{name}'"));
            }
            (_, Err(_)) => {
                self.invalid_header
                    .get_or_insert_with(|| format!("Invalid header value '{value}'"));
            }
        }
        self
    }

    /// Requests a WebSocket subprotocol during the handshake.
    pub fn subprotocol(mut self, subprotocol: impl Into<String>) -> Self {
        self.subprotocol = Some(subprotocol.into());
        self
    }

    /// Sets what each worker does on its connection.
    pub fn mode(mut self, mode: TestMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the time allowed for the TCP, TLS and WebSocket handshake.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the time an echo request waits for its reply.
    pub fn message_timeout(mut self, timeout: Duration) -> Self {
        self.message_timeout = timeout;
        self
    }

    /// Sends a keepalive ping every `interval`; zero disables pings.
    pub fn ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Sets how echo mode recognises the reply to a request.
    pub fn reply_match(mut self, reply_match: ReplyMatch) -> Self {
        self.reply_match = reply_match;
        self
    }

    /// Attaches SLO thresholds, validated against the mode by `build`.
    pub fn slo(mut self, slo: SloThresholds) -> Self {
        self.slo = slo;
        self
    }

    /// Sets the format [`render_report`](crate::render_report) produces.
    pub fn output_format(mut self, format: OutputFormat) -> Self {
        self.output_format = format;
        self
    }

    /// Draws a progress bar on stderr while the run is in progress.
    pub fn progress(mut self, enabled: bool) -> Self {
        self.progress = enabled;
        self
    }

    /// Controls whether workers reconnect, with backoff, after a timeout,
    /// I/O error or server close. Enabled by default.
    pub fn reconnect(mut self, enabled: bool) -> Self {
        self.reconnect = enabled;
        self
    }

    /// Records every data frame and connection event of the run to `path`
    /// as JSON lines, one record per line after a header line. The file is
    /// created, or truncated, when the run starts.
    ///
    /// Workers never wait for the disk: records past
    /// [`capture_limit`](Self::capture_limit), or produced while the writer
    /// is too far behind, are dropped and counted in
    /// [`AggregatedMetrics::capture`](crate::AggregatedMetrics::capture).
    /// The format is described in METHODOLOGY.md, section 11.
    #[cfg(feature = "capture")]
    #[cfg_attr(docsrs, doc(cfg(feature = "capture")))]
    pub fn capture(mut self, path: impl Into<PathBuf>) -> Self {
        self.capture = Some(path.into());
        self
    }

    /// Sets the most records [`capture`](Self::capture) writes; 0 means
    /// unlimited. Defaults to 100 000.
    #[cfg(feature = "capture")]
    #[cfg_attr(docsrs, doc(cfg(feature = "capture")))]
    pub fn capture_limit(mut self, records: u64) -> Self {
        self.capture_limit = records;
        self
    }

    /// Sets the GraphQL operation sent by [`TestMode::GraphQLWS`] workers.
    /// Required in that mode.
    #[cfg(feature = "graphql-ws")]
    #[cfg_attr(docsrs, doc(cfg(feature = "graphql-ws")))]
    pub fn graphql_operation(mut self, operation: impl Into<String>) -> Self {
        self.graphql.operation = Some(operation.into());
        self
    }

    /// Sets the GraphQL operation name.
    #[cfg(feature = "graphql-ws")]
    #[cfg_attr(docsrs, doc(cfg(feature = "graphql-ws")))]
    pub fn graphql_operation_name(mut self, name: impl Into<String>) -> Self {
        self.graphql.operation_name = Some(name.into());
        self
    }

    /// Sets the GraphQL variables. Must be a JSON object.
    #[cfg(feature = "graphql-ws")]
    #[cfg_attr(docsrs, doc(cfg(feature = "graphql-ws")))]
    pub fn graphql_variables(mut self, variables: serde_json::Value) -> Self {
        self.graphql.variables = Some(variables);
        self
    }

    /// Sets the GraphQL extensions. Must be a JSON object.
    #[cfg(feature = "graphql-ws")]
    #[cfg_attr(docsrs, doc(cfg(feature = "graphql-ws")))]
    pub fn graphql_extensions(mut self, extensions: serde_json::Value) -> Self {
        self.graphql.extensions = Some(extensions);
        self
    }

    /// Validates the settings and returns the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Config`] when the command line would reject the
    /// same settings:
    ///
    /// - [`ConfigError::InvalidUrl`] or [`ConfigError::UnsupportedScheme`] for
    ///   a target that does not parse or is not `ws://` or `wss://`.
    /// - [`ConfigError::ZeroConcurrency`] for zero connections, or for stages
    ///   whose targets are all zero.
    /// - [`ConfigError::InvalidOption`] for a stage with a zero duration.
    /// - [`ConfigError::UnsupportedOption`] for stages combined with a ramp
    ///   rate.
    /// - [`ConfigError::InvalidOption`] for a zero request limit, or for a
    ///   missing GraphQL operation or non-object GraphQL variables or
    ///   extensions in GraphQL mode.
    /// - [`ConfigError::InvalidHeader`] for a header name or value that is not
    ///   valid HTTP.
    /// - [`ConfigError::InvalidThreshold`] for an error-rate threshold outside
    ///   0.0 to 1.0.
    /// - [`ConfigError::UnsupportedOption`] for a latency threshold or a
    ///   non-default [`ReplyMatch`] outside [`TestMode::Echo`], a JSON reply
    ///   match whose pointer is invalid or whose payload lacks `{{seq}}`, or
    ///   GraphQL settings outside GraphQL mode.
    pub fn build(mut self) -> Result<LoadTestConfig> {
        let target_url = parse_target_url(&self.target)?;
        if !self.stages.is_empty() {
            validate_stages(&self.stages, self.ramp_rate)?;
            self.connections = self.stages.iter().map(|s| s.target).max().unwrap_or(0);
            self.duration = self.stages.iter().map(|s| s.duration).sum();
        }
        validate_connections(self.connections)?;
        validate_max_requests(self.max_requests)?;
        if let ReplyMatch::JsonPointer(pointer) = &self.reply_match
            && !is_json_pointer(pointer)
        {
            return Err(invalid_json_pointer(pointer).into());
        }
        #[cfg(feature = "graphql-ws")]
        let graphql_ws = validate_graphql(self.mode, self.graphql)?;
        validate_reply_match(&self.reply_match, self.mode, std::iter::once(&self.payload))?;
        if let Some(message) = self.invalid_header {
            return Err(ConfigError::InvalidHeader(message).into());
        }
        validate_slo(&self.slo, self.mode)?;

        Ok(LoadTestConfig {
            target_url,
            connections: self.connections,
            ramp_rate: self.ramp_rate,
            duration: self.duration,
            stages: self.stages,
            max_requests: self.max_requests,
            rate_per_conn: self.rate_per_conn,
            payload: self.payload,
            headers: self.headers,
            subprotocol: self.subprotocol,
            mode: self.mode,
            connect_timeout: self.connect_timeout,
            message_timeout: self.message_timeout,
            ping_interval: self.ping_interval,
            #[cfg(feature = "auth")]
            auth: None,
            #[cfg(feature = "auth")]
            live_auth: None,
            #[cfg(feature = "graphql-ws")]
            graphql_ws,
            slo: self.slo,
            output_format: self.output_format,
            output_path: None,
            tui: false,
            no_progress: !self.progress,
            no_reconnect: !self.reconnect,
            reply_match: self.reply_match,
            #[cfg(feature = "capture")]
            capture: self.capture.map(|path| crate::capture::CaptureConfig {
                path,
                limit: self.capture_limit,
            }),
            #[cfg(feature = "distributed")]
            stage_share: None,
        })
    }
}

fn validate_stages(stages: &[Stage], ramp_rate: u64) -> Result<()> {
    if ramp_rate > 0 {
        return Err(ConfigError::UnsupportedOption(
            "--stages sets the connection ramp itself and cannot be combined with --ramp-rate"
                .to_string(),
        )
        .into());
    }
    if let Some(index) = stages.iter().position(|s| s.duration.is_zero()) {
        return Err(ConfigError::InvalidOption(format!(
            "stage {} has a zero duration; every stage needs a duration greater than zero",
            index + 1
        ))
        .into());
    }
    Ok(())
}

/// One stage of a staged load profile: over `duration`, the number of open
/// connections moves linearly from the previous stage's target to `target`.
///
/// See [`LoadTestConfigBuilder::stages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stage {
    /// How long the stage lasts.
    pub duration: Duration,
    /// Connections open at the end of the stage.
    pub target: usize,
}

impl Stage {
    /// Creates a stage that reaches `target` connections after `duration`.
    pub fn new(duration: Duration, target: usize) -> Self {
        Self { duration, target }
    }

    /// Parses a comma-separated list of `DURATION:TARGET` stages, such as
    /// `30s:100,2m:100,30s:0`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] for an empty list or a stage
    /// that is not `DURATION:TARGET`, and [`ConfigError::InvalidDuration`] for
    /// a duration that does not parse.
    pub fn parse_list(spec: &str) -> Result<Vec<Stage>> {
        let stages = spec
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| {
                let invalid = || {
                    ConfigError::InvalidOption(format!(
                        "invalid stage '{part}': expected DURATION:CONNECTIONS, such as 30s:100"
                    ))
                };
                let (duration, target) = part.split_once(':').ok_or_else(invalid)?;
                let target = target.trim().parse::<usize>().map_err(|_| invalid())?;
                let duration = parse_human_duration(duration.trim())?;
                Ok(Stage::new(duration, target))
            })
            .collect::<Result<Vec<_>>>()?;
        if stages.is_empty() {
            return Err(ConfigError::InvalidOption(
                "--stages needs at least one DURATION:CONNECTIONS stage, such as 30s:100"
                    .to_string(),
            )
            .into());
        }
        Ok(stages)
    }

    /// The number of connections a staged run should have open `elapsed`
    /// after it started, rounded to the nearest connection.
    pub(crate) fn target_at(stages: &[Stage], elapsed: Duration) -> usize {
        let mut from = 0usize;
        let mut stage_start = Duration::ZERO;
        for stage in stages {
            let stage_end = stage_start + stage.duration;
            if elapsed < stage_end {
                let progress = (elapsed - stage_start).as_secs_f64() / stage.duration.as_secs_f64();
                let value = from as f64 + (stage.target as f64 - from as f64) * progress;
                return value.round() as usize;
            }
            from = stage.target;
            stage_start = stage_end;
        }
        from
    }
}

fn validate_connections(connections: usize) -> Result<()> {
    if connections == 0 {
        return Err(ConfigError::ZeroConcurrency(0).into());
    }
    Ok(())
}

fn validate_max_requests(max_requests: Option<u64>) -> Result<()> {
    if max_requests == Some(0) {
        return Err(ConfigError::InvalidOption(
            "requests limit (-n) must be greater than zero".to_string(),
        )
        .into());
    }
    Ok(())
}

/// Rejects an out-of-range error budget and latency budgets in modes that do
/// not measure round-trip time.
fn validate_slo(slo: &SloThresholds, mode: TestMode) -> Result<()> {
    validate_error_rate(slo)?;
    validate_latency_mode(slo, mode, "", "--max-p50/--max-p95/--max-p99/--max-p999")
}

/// Rejects an error budget outside 0.0 to 1.0.
fn validate_error_rate(slo: &SloThresholds) -> Result<()> {
    if let Some(err_rate) = slo.max_error_rate
        && !(0.0..=1.0).contains(&err_rate)
    {
        return Err(ConfigError::InvalidThreshold(format!(
            "max_error_rate must be between 0.0 and 1.0, received {err_rate}"
        ))
        .into());
    }
    Ok(())
}

/// Rejects latency budgets outside echo mode. `prefix` names where the
/// thresholds come from, such as a scenario phase, and `keys` the settings
/// that set them.
fn validate_latency_mode(
    slo: &SloThresholds,
    mode: TestMode,
    prefix: &str,
    keys: &str,
) -> Result<()> {
    // Only echo mode measures round-trip time; any other mode would leave the
    // RTT histogram empty and a latency budget could never be evaluated.
    if slo.has_latency() && mode != TestMode::Echo {
        return Err(ConfigError::UnsupportedOption(format!(
            "{prefix}latency SLOs ({keys}) require --mode echo; {mode:?} mode does not measure round-trip time"
        ))
        .into());
    }
    Ok(())
}

/// Rejects GraphQL options outside `graphql-ws` mode, and a missing operation
/// or non-object variables and extensions inside it.
#[cfg(feature = "graphql-ws")]
fn validate_graphql(mode: TestMode, options: GraphQLOptions) -> Result<Option<GraphQLWSConfig>> {
    let has_graphql_options = options.operation.is_some()
        || options.operation_name.is_some()
        || options.variables.is_some()
        || options.extensions.is_some();

    if mode != TestMode::GraphQLWS {
        if has_graphql_options {
            return Err(ConfigError::UnsupportedOption(
                "GraphQL WS options require --mode graphql-ws".to_string(),
            )
            .into());
        }
        return Ok(None);
    }

    let operation = options
        .operation
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ConfigError::InvalidOption(
                "--mode graphql-ws requires --graphql-ws-operation <QUERY>".to_string(),
            )
        })?;

    for (flag, value) in [
        ("--graphql-ws-variables", &options.variables),
        ("--graphql-ws-extensions", &options.extensions),
    ] {
        if value.as_ref().is_some_and(|value| !value.is_object()) {
            return Err(
                ConfigError::InvalidOption(format!("{flag} must contain a JSON object")).into(),
            );
        }
    }

    Ok(Some(GraphQLWSConfig {
        operation_name: options.operation_name,
        operation,
        variables: options.variables,
        extensions: options.extensions,
    }))
}

/// Rule an echo wait uses to recognise the reply to the request in flight.
///
/// Frames that do not match are counted as unsolicited and skipped; the wait
/// still ends at the message timeout. Parses from the `--match` spelling:
/// `first`, `exact` or `json:POINTER`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplyMatch {
    /// The first text or binary frame after the send is the reply.
    #[default]
    First,
    /// The reply is byte-identical to the frame that was sent.
    Exact,
    /// The JSON value at this RFC 6901 pointer equals the request's `{{seq}}`.
    JsonPointer(String),
}

impl ReplyMatch {
    /// Renders the rule as it is spelled on the command line.
    pub(crate) fn flag_value(&self) -> String {
        match self {
            Self::First => "first".to_string(),
            Self::Exact => "exact".to_string(),
            Self::JsonPointer(pointer) => format!("json:{pointer}"),
        }
    }
}

impl FromStr for ReplyMatch {
    type Err = ConfigError;

    /// Parses `first`, `exact` or `json:POINTER`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnsupportedOption`] for any other value or for a
    /// pointer that is not valid RFC 6901 syntax.
    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        match raw {
            "first" => Ok(Self::First),
            "exact" => Ok(Self::Exact),
            _ => match raw.strip_prefix("json:") {
                Some(pointer) if is_json_pointer(pointer) => {
                    Ok(Self::JsonPointer(pointer.to_string()))
                }
                Some(pointer) => Err(invalid_json_pointer(pointer)),
                None => Err(ConfigError::UnsupportedOption(format!(
                    "--match '{raw}' is not supported; use first, exact or json:<pointer>"
                ))),
            },
        }
    }
}

fn invalid_json_pointer(pointer: &str) -> ConfigError {
    ConfigError::UnsupportedOption(format!(
        "--match json:{pointer}: '{pointer}' is not a valid RFC 6901 JSON pointer \
         (use '' or '/'-separated tokens, escaping '~' as '~0' and '/' as '~1')"
    ))
}

/// Checks RFC 6901 syntax: empty, or `/`-prefixed tokens where `~` only
/// appears as the escapes `~0` and `~1`.
fn is_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

/// Rejects reply matching that the configured mode or payloads cannot honour.
///
/// JSON matching compares against the request's `{{seq}}`, so every payload a
/// worker may send must be a text template carrying that placeholder.
fn validate_reply_match<'a>(
    reply_match: &ReplyMatch,
    mode: TestMode,
    mut payloads: impl Iterator<Item = &'a PayloadConfig>,
) -> Result<()> {
    if *reply_match == ReplyMatch::First {
        return Ok(());
    }
    if mode != TestMode::Echo {
        return Err(ConfigError::UnsupportedOption(format!(
            "--match {} requires --mode echo; {mode:?} mode does not wait for replies",
            reply_match.flag_value()
        ))
        .into());
    }
    if let ReplyMatch::JsonPointer(_) = reply_match {
        let carries_seq = |payload: &PayloadConfig| match payload {
            PayloadConfig::DynamicText(template) => template.contains("{{seq}}"),
            PayloadConfig::StaticText(_) | PayloadConfig::Binary(_) => false,
        };
        if !payloads.all(carries_seq) {
            return Err(ConfigError::UnsupportedOption(format!(
                "--match {} needs a text payload containing the {{{{seq}}}} placeholder to compare replies against",
                reply_match.flag_value()
            ))
            .into());
        }
    }
    Ok(())
}

/// Payload specification supporting zero-allocation cached frames or dynamic templating.
#[derive(Debug, Clone)]
pub(crate) enum PayloadConfig {
    /// Static UTF-8 text cached as Utf8Bytes for O(1) zero-allocation cloning.
    StaticText(Utf8Bytes),
    /// Dynamic text template requiring macro substitutions on each frame dispatch.
    DynamicText(String),
    /// Binary frame payload cached as Bytes for O(1) zero-allocation cloning.
    Binary(Bytes),
}

impl PayloadConfig {
    /// Classifies text payload into either a pre-allocated static frame or a dynamic template.
    pub(crate) fn from_text(text: String) -> Self {
        if text.contains("{{") {
            Self::DynamicText(text)
        } else {
            Self::StaticText(Utf8Bytes::from(text))
        }
    }
}

/// Service level objectives checked by [`evaluate_slos`](crate::evaluate_slos).
///
/// Every threshold is optional; an unset threshold is not checked. Latency
/// thresholds apply to message round-trip time, which only
/// [`TestMode::Echo`] measures.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct SloThresholds {
    /// Highest acceptable median round-trip time.
    pub max_p50: Option<Duration>,
    /// Highest acceptable 95th percentile round-trip time.
    pub max_p95: Option<Duration>,
    /// Highest acceptable 99th percentile round-trip time.
    pub max_p99: Option<Duration>,
    /// Highest acceptable 99.9th percentile round-trip time.
    pub max_p999: Option<Duration>,
    /// Highest acceptable error rate, as a fraction between 0.0 and 1.0.
    pub max_error_rate: Option<f64>,
    /// Lowest acceptable throughput in messages per second.
    pub min_throughput: Option<f64>,
}

impl SloThresholds {
    /// Returns thresholds with nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the highest acceptable median round-trip time.
    #[must_use]
    pub fn with_max_p50(mut self, max: Duration) -> Self {
        self.max_p50 = Some(max);
        self
    }

    /// Sets the highest acceptable 95th percentile round-trip time.
    #[must_use]
    pub fn with_max_p95(mut self, max: Duration) -> Self {
        self.max_p95 = Some(max);
        self
    }

    /// Sets the highest acceptable 99th percentile round-trip time.
    #[must_use]
    pub fn with_max_p99(mut self, max: Duration) -> Self {
        self.max_p99 = Some(max);
        self
    }

    /// Sets the highest acceptable 99.9th percentile round-trip time.
    #[must_use]
    pub fn with_max_p999(mut self, max: Duration) -> Self {
        self.max_p999 = Some(max);
        self
    }

    /// Sets the highest acceptable error rate, a fraction between 0.0 and 1.0.
    #[must_use]
    pub fn with_max_error_rate(mut self, max: f64) -> Self {
        self.max_error_rate = Some(max);
        self
    }

    /// Sets the lowest acceptable throughput in messages per second.
    #[must_use]
    pub fn with_min_throughput(mut self, min: f64) -> Self {
        self.min_throughput = Some(min);
        self
    }

    /// True when a latency threshold is set.
    pub(crate) fn has_latency(&self) -> bool {
        self.max_p50.is_some()
            || self.max_p95.is_some()
            || self.max_p99.is_some()
            || self.max_p999.is_some()
    }

    /// These thresholds, with every unset one taken from `fallback`.
    #[cfg(feature = "scenario")]
    pub(crate) fn or_from(&self, fallback: &Self) -> Self {
        Self {
            max_p50: self.max_p50.or(fallback.max_p50),
            max_p95: self.max_p95.or(fallback.max_p95),
            max_p99: self.max_p99.or(fallback.max_p99),
            max_p999: self.max_p999.or(fallback.max_p999),
            max_error_rate: self.max_error_rate.or(fallback.max_error_rate),
            min_throughput: self.min_throughput.or(fallback.min_throughput),
        }
    }
}

/// Handshake authentication from the `--auth-*` flags.
#[cfg(feature = "auth")]
#[derive(Debug, Clone)]
pub(crate) struct AuthConfig {
    pub(crate) scheme: AuthScheme,
    /// Name of the header that carries the credential.
    pub(crate) header: String,
    pub(crate) credential: Credential,
}

/// How the credential is sent.
#[cfg(feature = "auth")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScheme {
    /// A token, sent as `Bearer <token>` in `Authorization` and as is in any
    /// other header.
    Static,
    /// An API key, sent as is.
    ApiKey,
}

/// Where the credential comes from.
#[cfg(feature = "auth")]
#[derive(Debug, Clone)]
pub(crate) enum Credential {
    /// Given on the command line.
    Value(Secret),
    /// Printed by a command before the run, and optionally during it.
    Command(TokenCommand),
}

/// A credential whose `Debug` output is redacted.
#[cfg(feature = "auth")]
#[derive(Clone)]
pub(crate) struct Secret(String);

#[cfg(feature = "auth")]
impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "auth")]
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// `--auth-token-cmd` and its timing options.
#[cfg(feature = "auth")]
#[derive(Debug, Clone)]
pub(crate) struct TokenCommand {
    /// Run through the platform shell; its trimmed stdout is the token.
    pub(crate) command: String,
    /// Time allowed for each run of the command.
    pub(crate) timeout: Duration,
    /// Interval between runs during the load test, if the token is refreshed.
    pub(crate) refresh: Option<Duration>,
}

#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone)]
pub(crate) struct GraphQLWSConfig {
    pub(crate) operation_name: Option<String>,
    pub(crate) operation: String,
    pub(crate) variables: Option<serde_json::Value>,
    pub(crate) extensions: Option<serde_json::Value>,
}

#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum GraphQLWSMessage {
    #[serde(rename = "connection_init")]
    ConnectionInit { payload: Option<serde_json::Value> },
    #[serde(rename = "connection_ack")]
    ConnectionAck,
    #[serde(rename = "connection_error")]
    ConnectionError { payload: Option<serde_json::Value> },
    #[serde(rename = "ka")]
    ConnectionKeepAlive,
    #[serde(rename = "connection_terminate")]
    ConnectionTerminate,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "start")]
    Start {
        id: String,
        payload: GraphQLWSStartPayload,
    },
    #[serde(rename = "stop")]
    Stop { id: String },
    #[serde(rename = "subscribe")]
    Subscribe {
        id: String,
        payload: GraphQLWSStartPayload,
    },
    #[serde(rename = "data")]
    Data {
        id: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "next")]
    Next {
        id: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "error")]
    Error {
        id: String,
        payload: Vec<serde_json::Value>,
    },
    #[serde(rename = "complete")]
    Complete { id: String },
}

#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GraphQLWSStartPayload {
    pub(crate) query: String,
    #[serde(rename = "operationName")]
    pub(crate) operation_name: Option<String>,
    pub(crate) variables: Option<serde_json::Value>,
    pub(crate) extensions: Option<serde_json::Value>,
}

/// A multi-phase load scenario, parsed and validated from YAML.
///
/// Load one with [`ScenarioConfig::load`] or [`ScenarioConfig::from_yaml`] and
/// run it with [`ScenarioExecutor`](crate::ScenarioExecutor). The file format
/// is described in [docs/scenarios.md].
///
/// [docs/scenarios.md]: https://github.com/jephter-olamiposi/wsblast/blob/main/docs/scenarios.md
#[cfg(feature = "scenario")]
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioConfig {
    pub(crate) version: String,
    /// Optional target endpoint for every phase. CLI `--url` takes precedence.
    #[serde(default)]
    pub(crate) target: Option<String>,
    pub(crate) metadata: ScenarioMeta,
    /// SLOs checked on the metrics of the whole scenario.
    #[serde(default)]
    pub(crate) slo: Option<SloConfig>,
    pub(crate) phases: Vec<PhaseConfig>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScenarioMeta {
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) description: Option<String>,
    #[allow(dead_code)]
    pub(crate) tags: Option<Vec<String>>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseConfig {
    pub(crate) name: String,
    /// Optional endpoint override for this phase.
    #[serde(default)]
    pub(crate) target: Option<String>,
    /// Required unless `stages` is set, which excludes it.
    #[serde(default)]
    pub(crate) duration: Option<String>,
    /// Required unless `stages` is set, which excludes it.
    #[serde(default)]
    pub(crate) connections: Option<usize>,
    /// Excluded by `stages`; 0 when absent.
    #[serde(default)]
    pub(crate) ramp_rate: Option<u64>,
    /// A staged load profile in place of `connections`, `duration` and
    /// `ramp_rate`.
    #[serde(default)]
    pub(crate) stages: Option<StagesConfig>,
    #[serde(default)]
    pub(crate) rate_per_conn: u64,
    #[serde(default)]
    pub(crate) payload: Option<String>,
    #[serde(default)]
    pub(crate) think_time: Option<String>,
    #[serde(default)]
    pub(crate) actions: Vec<ActionConfig>,
    /// SLOs checked on this phase's own metrics.
    #[serde(default)]
    pub(crate) slo: Option<SloConfig>,
}

/// The `stages:` of a phase: a list of `{duration, target}` mappings, or one
/// string in the `--stages` syntax (`30s:100,1m:100,30s:0`).
#[cfg(feature = "scenario")]
#[derive(Debug, Clone)]
pub(crate) enum StagesConfig {
    /// The `--stages` syntax.
    Spec(String),
    /// One mapping per stage.
    List(Vec<StageConfig>),
}

/// One stage of a phase's `stages:` list.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageConfig {
    pub(crate) duration: String,
    pub(crate) target: usize,
}

/// Reads a string or a sequence, so each form keeps the error messages of
/// its own parser: an unknown key in a stage mapping is reported by name.
#[cfg(feature = "scenario")]
impl<'de> Deserialize<'de> for StagesConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct StagesVisitor;

        impl<'de> serde::de::Visitor<'de> for StagesVisitor {
            type Value = StagesConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a list of {duration, target} stages or a string such as \"30s:100,1m:100,30s:0\"",
                )
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StagesConfig::Spec(value.to_owned()))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut stages = Vec::new();
                while let Some(stage) = seq.next_element::<StageConfig>()? {
                    stages.push(stage);
                }
                Ok(StagesConfig::List(stages))
            }
        }

        deserializer.deserialize_any(StagesVisitor)
    }
}

#[cfg(feature = "scenario")]
impl StagesConfig {
    /// Parses and validates the stages of phase `phase`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] for an empty list, a stage
    /// that does not parse or has a zero duration, and stages whose targets
    /// are all zero.
    fn compile(&self, phase: &str) -> Result<Vec<Stage>> {
        let stages = match self {
            Self::Spec(spec) if spec.split(',').all(|part| part.trim().is_empty()) => Vec::new(),
            Self::Spec(spec) => Stage::parse_list(spec).map_err(|e| {
                ConfigError::InvalidScenario(format!("phase '{phase}': stages: {e}"))
            })?,
            Self::List(list) => list
                .iter()
                .enumerate()
                .map(|(index, stage)| {
                    let duration = parse_human_duration(&stage.duration).map_err(|e| {
                        ConfigError::InvalidScenario(format!(
                            "phase '{phase}': stage {}: duration: {e}",
                            index + 1
                        ))
                    })?;
                    Ok(Stage::new(duration, stage.target))
                })
                .collect::<Result<Vec<_>>>()?,
        };
        if stages.is_empty() {
            return Err(invalid_phase(
                phase,
                "stages must list at least one stage, such as `{duration: 30s, target: 100}`",
            )
            .into());
        }
        if let Some(index) = stages.iter().position(|s| s.duration.is_zero()) {
            return Err(ConfigError::InvalidScenario(format!(
                "phase '{phase}': stage {} has a zero duration; every stage needs a duration greater than zero",
                index + 1
            ))
            .into());
        }
        if stages.iter().all(|s| s.target == 0) {
            return Err(invalid_phase(
                phase,
                "every stage target is zero; at least one stage needs connections",
            )
            .into());
        }
        Ok(stages)
    }
}

/// The `slo:` mapping of a scenario or a phase, with the thresholds of the
/// SLO flags of the same names.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SloConfig {
    #[serde(default)]
    pub(crate) max_p50: Option<String>,
    #[serde(default)]
    pub(crate) max_p95: Option<String>,
    #[serde(default)]
    pub(crate) max_p99: Option<String>,
    #[serde(default)]
    pub(crate) max_p999: Option<String>,
    #[serde(default)]
    pub(crate) max_error_rate: Option<f64>,
    #[serde(default)]
    pub(crate) min_throughput: Option<f64>,
}

#[cfg(feature = "scenario")]
impl SloConfig {
    /// Parses the thresholds. `context` prefixes every error, such as
    /// `phase 'spike': slo`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] for a duration that does not
    /// parse or an error rate outside 0.0 to 1.0.
    fn compile(&self, context: &str) -> Result<SloThresholds> {
        let invalid = |e: WsBlastError| ConfigError::InvalidScenario(format!("{context}: {e}"));
        let latency = |key: &str, raw: &Option<String>| {
            raw.as_deref()
                .map(|raw| {
                    parse_human_duration(raw)
                        .map_err(|e| ConfigError::InvalidScenario(format!("{context}: {key}: {e}")))
                })
                .transpose()
        };
        let slo = SloThresholds {
            max_p50: latency("max_p50", &self.max_p50)?,
            max_p95: latency("max_p95", &self.max_p95)?,
            max_p99: latency("max_p99", &self.max_p99)?,
            max_p999: latency("max_p999", &self.max_p999)?,
            max_error_rate: self.max_error_rate,
            min_throughput: self.min_throughput,
        };
        validate_error_rate(&slo).map_err(invalid)?;
        Ok(slo)
    }
}

/// Parses an optional `slo:` mapping into thresholds, empty when absent.
#[cfg(feature = "scenario")]
fn compile_slo(slo: Option<&SloConfig>, context: &str) -> Result<SloThresholds> {
    slo.map_or_else(|| Ok(SloThresholds::default()), |slo| slo.compile(context))
}

/// The keys of a scenario `slo:` mapping that set latency thresholds.
#[cfg(feature = "scenario")]
const SLO_LATENCY_KEYS: &str = "max_p50/max_p95/max_p99/max_p999";

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub(crate) enum ActionConfig {
    Send {
        payload: Option<String>,
        #[serde(default)]
        count: Option<u64>,
    },
    Wait {
        duration: String,
    },
    Close {
        code: Option<u16>,
        reason: Option<String>,
    },
    Ping {
        interval: String,
    },
}

/// Validated, execution-ready phase compiled from YAML.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone)]
pub(crate) struct ScenarioPhase {
    pub(crate) name: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) target: Option<Url>,
    pub(crate) duration: Duration,
    pub(crate) connections: usize,
    pub(crate) ramp_rate: u64,
    pub(crate) rate_per_conn: u64,
    pub(crate) payload: Option<PayloadConfig>,
    pub(crate) think_time: Duration,
    pub(crate) actions: Vec<ScenarioAction>,
    /// Thresholds checked on this phase's own metrics.
    pub(crate) slo: SloThresholds,
    /// The phase's staged load profile; empty for a constant connection
    /// count. With stages, `connections` is the highest target, `duration`
    /// the sum of the stage durations and `ramp_rate` 0.
    pub(crate) stages: Vec<Stage>,
}

/// Execution-ready action with parsed durations and payload templates.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone)]
pub(crate) enum ScenarioAction {
    Send {
        payload: Option<PayloadConfig>,
        count: u64,
    },
    Wait(Duration),
    Ping(Duration),
    Close {
        code: u16,
        reason: String,
    },
}

#[cfg(feature = "scenario")]
impl ScenarioConfig {
    /// Reads, parses, and validates a scenario file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::PayloadFile`] when the file cannot be read and
    /// [`ConfigError::InvalidScenario`] when YAML or semantic validation fails.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|e| ConfigError::PayloadFile {
            path: path.display().to_string(),
            source: e,
        })?;
        Self::parse(&content, Some(path))
    }

    /// Parses and validates a scenario from YAML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] when the YAML does not parse,
    /// has unknown keys, or fails validation.
    ///
    /// # Examples
    ///
    /// ```
    /// use wsblast::ScenarioConfig;
    ///
    /// # fn main() -> wsblast::Result<()> {
    /// let scenario = ScenarioConfig::from_yaml(
    ///     r#"
    /// version: "1.0"
    /// target: "ws://127.0.0.1:9001"
    /// metadata:
    ///   name: "warmup then sustain"
    /// phases:
    ///   - name: "warmup"
    ///     duration: "5s"
    ///     connections: 10
    ///   - name: "sustain"
    ///     duration: "30s"
    ///     connections: 50
    ///     rate_per_conn: 100
    /// "#,
    /// )?;
    /// assert_eq!(scenario.name(), "warmup then sustain");
    /// assert_eq!(scenario.phase_names().collect::<Vec<_>>(), ["warmup", "sustain"]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        Self::parse(yaml, None)
    }

    fn parse(yaml: &str, path: Option<&Path>) -> Result<Self> {
        let scenario: Self = serde_norway::from_str(yaml).map_err(|e| {
            ConfigError::InvalidScenario(match path {
                Some(path) => format!("{}: {e}", path.display()),
                None => e.to_string(),
            })
        })?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Scenario name from `metadata.name`.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// Phase names in run order.
    pub fn phase_names(&self) -> impl Iterator<Item = &str> {
        self.phases.iter().map(|phase| phase.name.as_str())
    }

    /// Validates scenario metadata, phase uniqueness, and every phase action.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != "1.0" {
            return Err(ConfigError::InvalidScenario(format!(
                "unsupported version '{}', expected '1.0'",
                self.version
            ))
            .into());
        }
        if self.metadata.name.trim().is_empty() {
            return Err(ConfigError::InvalidScenario(
                "metadata.name must not be empty".to_string(),
            )
            .into());
        }
        if let Some(target) = &self.target {
            parse_target_url(target)?;
        }
        self.slo()?;
        if self.phases.is_empty() {
            return Err(ConfigError::InvalidScenario(
                "scenario must define at least one phase".to_string(),
            )
            .into());
        }

        let mut names = std::collections::HashSet::with_capacity(self.phases.len());
        for phase in &self.phases {
            if phase.name.trim().is_empty() {
                return Err(ConfigError::InvalidScenario(
                    "phase name must not be empty".to_string(),
                )
                .into());
            }
            if !names.insert(phase.name.as_str()) {
                return Err(ConfigError::InvalidScenario(format!(
                    "duplicate phase name '{}'",
                    phase.name
                ))
                .into());
            }
            ScenarioPhase::compile(phase)?;
        }

        Ok(())
    }

    /// The thresholds of the top-level `slo:` mapping, checked on the
    /// metrics of the whole scenario; empty when the file sets none.
    pub(crate) fn slo(&self) -> Result<SloThresholds> {
        compile_slo(self.slo.as_ref(), "slo")
    }

    /// Rejects latency SLOs, at the top level or on a phase that `selected`
    /// runs, in a mode that does not measure round-trip time.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnsupportedOption`] naming the first `slo:`
    /// mapping with a latency threshold outside echo mode.
    pub(crate) fn validate_slo_mode(&self, mode: TestMode, selected: Option<&str>) -> Result<()> {
        validate_latency_mode(&self.slo()?, mode, "slo: ", SLO_LATENCY_KEYS)?;
        for phase in self.compile(selected)? {
            validate_latency_mode(
                &phase.slo,
                mode,
                &format!("phase '{}': slo: ", phase.name),
                SLO_LATENCY_KEYS,
            )?;
        }
        Ok(())
    }

    /// Resolves the requested phase, defaulting to the first declared phase.
    pub(crate) fn selected_phase(&self, name: Option<&str>) -> Result<&PhaseConfig> {
        match name {
            Some(name) => self
                .phases
                .iter()
                .find(|phase| phase.name == name)
                .ok_or_else(|| {
                    ConfigError::InvalidScenario(format!(
                        "phase '{name}' not found; available phases: {}",
                        self.phases
                            .iter()
                            .map(|phase| phase.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .into()
                }),
            None => self.phases.first().ok_or_else(|| {
                ConfigError::InvalidScenario("scenario must define at least one phase".to_string())
                    .into()
            }),
        }
    }

    /// Resolves the target of every phase that `selected` runs, in run order.
    ///
    /// For each phase the target is the CLI URL, then the phase's `target`,
    /// then the scenario's `target`. A phase is never given another phase's
    /// endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] naming the first phase that
    /// has no target from any of the three sources, and URL errors for an
    /// unparsable or non-WebSocket target.
    pub(crate) fn resolve_phase_targets(
        &self,
        cli_url: Option<&str>,
        selected: Option<&str>,
    ) -> Result<Vec<Url>> {
        let phases = match selected {
            Some(_) => vec![self.selected_phase(selected)?],
            None => self.phases.iter().collect(),
        };
        phases
            .into_iter()
            .map(|phase| {
                let raw = cli_url
                    .or(phase.target.as_deref())
                    .or(self.target.as_deref())
                    .ok_or_else(|| {
                        invalid_phase(
                            &phase.name,
                            "no target URL; set `target` on this phase or at the top level of \
                             the scenario, or pass a URL on the command line",
                        )
                    })?;
                parse_target_url(raw)
            })
            .collect()
    }

    /// Compiles every phase, or only the named phase when `selected` is set.
    pub(crate) fn compile(&self, selected: Option<&str>) -> Result<Vec<ScenarioPhase>> {
        match selected {
            Some(_) => Ok(vec![ScenarioPhase::compile(
                self.selected_phase(selected)?,
            )?]),
            None => self.phases.iter().map(ScenarioPhase::compile).collect(),
        }
    }
}

#[cfg(feature = "scenario")]
impl ScenarioPhase {
    /// Parses and validates a raw phase definition.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] when durations, connection
    /// counts, stages, action counts, or close codes are invalid, when
    /// `connections` or `duration` is missing without `stages`, and when
    /// `stages` is combined with them or with `ramp_rate`.
    pub(crate) fn compile(phase: &PhaseConfig) -> Result<Self> {
        let (connections, duration, ramp_rate, stages) = match &phase.stages {
            Some(stages) => {
                let mixed: Vec<&str> = [
                    ("`connections`", phase.connections.is_some()),
                    ("`duration`", phase.duration.is_some()),
                    ("`ramp_rate`", phase.ramp_rate.is_some()),
                ]
                .into_iter()
                .filter_map(|(key, set)| set.then_some(key))
                .collect();
                if !mixed.is_empty() {
                    return Err(invalid_phase(
                        &phase.name,
                        &format!(
                            "`stages` cannot be combined with {}; the stages set the connection \
                             count, the duration and the ramp",
                            mixed.join(", ")
                        ),
                    )
                    .into());
                }
                let stages = stages.compile(&phase.name)?;
                let connections = stages.iter().map(|s| s.target).max().unwrap_or(0);
                let duration = stages.iter().map(|s| s.duration).sum();
                (connections, duration, 0, stages)
            }
            None => {
                let missing = |key: &str| {
                    invalid_phase(
                        &phase.name,
                        &format!(
                            "missing field `{key}`; give `connections` and `duration`, or `stages`"
                        ),
                    )
                };
                let connections = phase.connections.ok_or_else(|| missing("connections"))?;
                let raw_duration = phase
                    .duration
                    .as_deref()
                    .ok_or_else(|| missing("duration"))?;
                if connections == 0 {
                    return Err(invalid_phase(
                        &phase.name,
                        "connections must be greater than zero",
                    )
                    .into());
                }
                let duration = parse_human_duration(raw_duration).map_err(|e| {
                    ConfigError::InvalidScenario(format!("phase '{}': {e}", phase.name))
                })?;
                if duration.is_zero() {
                    return Err(
                        invalid_phase(&phase.name, "duration must be greater than zero").into(),
                    );
                }
                (
                    connections,
                    duration,
                    phase.ramp_rate.unwrap_or(0),
                    Vec::new(),
                )
            }
        };

        let think_time = match &phase.think_time {
            Some(raw) => parse_human_duration(raw).map_err(|e| {
                ConfigError::InvalidScenario(format!("phase '{}': think_time: {e}", phase.name))
            })?,
            None => Duration::ZERO,
        };

        let actions = phase
            .actions
            .iter()
            .map(|action| ScenarioAction::compile(&phase.name, action))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            name: phase.name.clone(),
            target: phase.target.as_deref().map(parse_target_url).transpose()?,
            duration,
            connections,
            ramp_rate,
            rate_per_conn: phase.rate_per_conn,
            payload: phase
                .payload
                .as_ref()
                .map(|payload| PayloadConfig::from_text(payload.clone())),
            think_time,
            actions,
            slo: compile_slo(phase.slo.as_ref(), &format!("phase '{}': slo", phase.name))?,
            stages,
        })
    }
}

#[cfg(feature = "scenario")]
impl ScenarioAction {
    /// Parses and validates one raw action.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`] for unparsable durations, zero
    /// send counts, or close codes that RFC 6455 forbids endpoints to send.
    pub(crate) fn compile(phase_name: &str, action: &ActionConfig) -> Result<Self> {
        match action {
            ActionConfig::Send { payload, count } => {
                let count = count.unwrap_or(1);
                if count == 0 {
                    return Err(invalid_phase(
                        phase_name,
                        "send action count must be greater than zero",
                    )
                    .into());
                }
                Ok(Self::Send {
                    payload: payload
                        .as_ref()
                        .map(|payload| PayloadConfig::from_text(payload.clone())),
                    count,
                })
            }
            ActionConfig::Wait { duration } => {
                let duration = parse_human_duration(duration).map_err(|e| {
                    ConfigError::InvalidScenario(format!("phase '{phase_name}': wait action: {e}"))
                })?;
                if duration.is_zero() {
                    return Err(invalid_phase(
                        phase_name,
                        "wait action duration must be greater than zero",
                    )
                    .into());
                }
                Ok(Self::Wait(duration))
            }
            ActionConfig::Ping { interval } => {
                let interval = parse_human_duration(interval).map_err(|e| {
                    ConfigError::InvalidScenario(format!("phase '{phase_name}': ping action: {e}"))
                })?;
                if interval.is_zero() {
                    return Err(invalid_phase(
                        phase_name,
                        "ping action interval must be greater than zero",
                    )
                    .into());
                }
                Ok(Self::Ping(interval))
            }
            ActionConfig::Close { code, reason } => {
                let code = validate_close_code(code.unwrap_or(1000))?;
                Ok(Self::Close {
                    code,
                    reason: reason.clone().unwrap_or_default(),
                })
            }
        }
    }
}

/// Parses a target URL and rejects schemes other than `ws` and `wss`.
pub(crate) fn parse_target_url(raw: &str) -> Result<Url> {
    let url =
        Url::parse(raw).map_err(|e| ConfigError::InvalidUrl(raw.to_string(), e.to_string()))?;
    match url.scheme() {
        "ws" | "wss" => Ok(url),
        other => Err(ConfigError::UnsupportedScheme(other.to_string()).into()),
    }
}

#[cfg(feature = "scenario")]
fn invalid_phase(phase: &str, message: &str) -> ConfigError {
    ConfigError::InvalidScenario(format!("phase '{phase}': {message}"))
}

/// Accepts the close codes an endpoint may send: 1000-1003 and 1007-1011
/// from RFC 6455, 1012-1014 from the IANA registry, and the library and
/// application ranges 3000-4999. RFC 6455 forbids sending 1004-1006 and
/// 1015, and reserves 1016-2999 for future protocol use.
#[cfg(feature = "scenario")]
fn validate_close_code(code: u16) -> Result<u16> {
    match code {
        1000..=1003 | 1007..=1014 | 3000..=4999 => Ok(code),
        _ => Err(ConfigError::InvalidScenario(format!(
            "close code {code} cannot be sent; use 1000-1003, 1007-1014 or 3000-4999"
        ))
        .into()),
    }
}

#[cfg(feature = "graphql-ws")]
fn parse_graphql_json_object(flag: &str, raw: Option<&str>) -> Result<Option<serde_json::Value>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| ConfigError::InvalidOption(format!("{flag} must contain valid JSON: {e}")))?;
    if !value.is_object() {
        return Err(
            ConfigError::InvalidOption(format!("{flag} must contain a JSON object")).into(),
        );
    }
    Ok(Some(value))
}

/// Validates GraphQL WS flags and compiles the connection payload.
///
/// GraphQL options are rejected outside `graphql-ws` mode instead of being
/// silently ignored, and the operation query is mandatory in that mode.
#[cfg(feature = "graphql-ws")]
fn build_graphql_config(cli: &Cli) -> Result<Option<GraphQLWSConfig>> {
    let has_graphql_options = cli.graphql_ws_operation.is_some()
        || cli.graphql_ws_operation_name.is_some()
        || cli.graphql_ws_variables.is_some()
        || cli.graphql_ws_extensions.is_some();

    if cli.mode != TestMode::GraphQLWS {
        if has_graphql_options {
            return Err(ConfigError::UnsupportedOption(
                "GraphQL WS options require --mode graphql-ws".to_string(),
            )
            .into());
        }
        return Ok(None);
    }

    let operation = cli
        .graphql_ws_operation
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ConfigError::InvalidOption(
                "--mode graphql-ws requires --graphql-ws-operation <QUERY>".to_string(),
            )
        })?;

    Ok(Some(GraphQLWSConfig {
        operation_name: cli.graphql_ws_operation_name.clone(),
        operation: operation.to_string(),
        variables: parse_graphql_json_object(
            "--graphql-ws-variables",
            cli.graphql_ws_variables.as_deref(),
        )?,
        extensions: parse_graphql_json_object(
            "--graphql-ws-extensions",
            cli.graphql_ws_extensions.as_deref(),
        )?,
    }))
}

impl LoadTestConfig {
    /// Constructs and validates a `LoadTestConfig` from parsed CLI arguments.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Config`] if:
    /// - No target URL is provided or URL scheme is not `ws://` or `wss://`.
    /// - Concurrency is zero.
    /// - Any duration string fails parsing.
    /// - Custom headers violate HTTP specification.
    /// - Error rate threshold is not within the `[0.0, 1.0]` range.
    /// - Payload file cannot be read or contains non-UTF-8 data for text mode.
    pub(crate) fn from_cli(cli: Cli) -> Result<Self> {
        Self::from_cli_with_target(cli, None)
    }

    /// Builds configuration from CLI arguments, optionally overriding the
    /// target URL with a value from a scenario file.
    ///
    /// The flags are converted into a [`LoadTestConfigBuilder`], so the CLI
    /// and the library share one validation path. Checks that `build` also
    /// runs are repeated here in flag order, so a command line with several
    /// mistakes reports the same one first as before.
    fn from_cli_with_target(mut cli: Cli, target_override: Option<String>) -> Result<Self> {
        let raw_url = target_override
            .or_else(|| cli.resolve_url().map(ToOwned::to_owned))
            .ok_or_else(|| {
                ConfigError::InvalidUrl(
                    "".into(),
                    "No target URL provided. Specify target as argument or --url <URL>".into(),
                )
            })?;

        parse_target_url(&raw_url)?;
        validate_connections(cli.connections)?;
        validate_max_requests(cli.requests)?;

        let duration = parse_human_duration(&cli.duration)?;
        let connect_timeout = parse_human_duration(&cli.connect_timeout)?;
        let message_timeout = parse_human_duration(&cli.message_timeout)?;
        let ping_interval = parse_human_duration(&cli.ping_interval)?;
        let reply_match: ReplyMatch = cli.reply_match.parse()?;

        #[cfg(feature = "graphql-ws")]
        let graphql = build_graphql_config(&cli)?;

        let payload = if let Some(path) = &cli.payload_file {
            let bytes = fs::read(path).map_err(|e| ConfigError::PayloadFile {
                path: path.display().to_string(),
                source: e,
            })?;
            if cli.binary {
                PayloadConfig::Binary(Bytes::from(bytes))
            } else {
                let text = String::from_utf8(bytes).map_err(|e| ConfigError::PayloadFile {
                    path: path.display().to_string(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                })?;
                PayloadConfig::from_text(text)
            }
        } else if let Some(text) = cli.payload.take() {
            if cli.binary {
                PayloadConfig::Binary(Bytes::from(text.into_bytes()))
            } else {
                PayloadConfig::from_text(text)
            }
        } else if cli.binary {
            PayloadConfig::Binary(Bytes::from_static(DEFAULT_BINARY_PAYLOAD))
        } else {
            PayloadConfig::from_text(DEFAULT_TEXT_PAYLOAD.to_string())
        };
        validate_reply_match(&reply_match, cli.mode, std::iter::once(&payload))?;

        let mut headers = HeaderMap::new();
        for h in &cli.headers {
            let (name, value) = parse_header_pair(h)?;
            headers.append(name, value);
        }

        let slo = SloThresholds {
            max_p50: cli
                .max_p50
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p95: cli
                .max_p95
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p99: cli
                .max_p99
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_p999: cli
                .max_p999
                .as_deref()
                .map(parse_human_duration)
                .transpose()?,
            max_error_rate: cli.max_error_rate,
            min_throughput: cli.min_throughput,
        };

        let stages = cli
            .stages
            .as_deref()
            .map(Stage::parse_list)
            .transpose()?
            .unwrap_or_default();
        let mut builder = LoadTestConfig::builder(raw_url)
            .connections(cli.connections)
            .ramp_rate(cli.ramp_rate)
            .duration(duration)
            .stages(stages)
            .rate_per_connection(cli.rate)
            .mode(cli.mode)
            .connect_timeout(connect_timeout)
            .message_timeout(message_timeout)
            .ping_interval(ping_interval)
            .reply_match(reply_match)
            .slo(slo)
            .output_format(cli.format)
            .progress(!cli.no_progress)
            .reconnect(!cli.no_reconnect);
        builder.max_requests = cli.requests;
        #[cfg(feature = "capture")]
        {
            builder.capture = cli.capture.take();
            builder.capture_limit = cli.capture_limit;
        }
        builder.payload = payload;
        builder.headers = headers;
        builder.subprotocol = cli.subprotocol.take();
        #[cfg(feature = "graphql-ws")]
        if let Some(graphql) = graphql {
            builder.graphql = GraphQLOptions {
                operation: Some(graphql.operation),
                operation_name: graphql.operation_name,
                variables: graphql.variables,
                extensions: graphql.extensions,
            };
        }

        let mut config = builder.build()?;
        config.output_path = cli.output_file.take();
        config.tui = cli.tui;
        #[cfg(feature = "auth")]
        {
            config.auth = auth_from_cli(&cli)?;
        }
        Ok(config)
    }

    /// Builds a validated configuration from an already parsed scenario.
    ///
    /// Phase-specific sizing, payload, and pacing override the CLI defaults;
    /// authentication, headers, timeouts, SLOs, and output settings continue to
    /// come from the CLI.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the requested phase does not exist, when
    /// phase values are invalid, or when CLI validation fails.
    #[cfg(feature = "scenario")]
    pub(crate) fn from_scenario_config(scenario: &ScenarioConfig, cli: &Cli) -> Result<Self> {
        if cli.stages.is_some() {
            return Err(ConfigError::UnsupportedOption(
                "--stages cannot be used with --scenario; give each scenario phase its own connections and ramp_rate, or its own stages"
                    .to_string(),
            )
            .into());
        }
        if cli.tui {
            return Err(ConfigError::UnsupportedOption(
                "the interactive TUI is not supported with --scenario; use the standard progress output"
                    .to_string(),
            )
            .into());
        }

        let phase = ScenarioPhase::compile(scenario.selected_phase(cli.phase.as_deref())?)?;

        let target = scenario
            .resolve_phase_targets(cli.resolve_url(), cli.phase.as_deref())?
            .into_iter()
            .next()
            .map(String::from);

        let mut config = Self::from_cli_with_target(cli.clone(), target)?;
        scenario.validate_slo_mode(config.mode, cli.phase.as_deref())?;

        config.connections = phase.connections;
        config.duration = phase.duration;
        config.ramp_rate = phase.ramp_rate;
        config.rate_per_conn = phase.rate_per_conn;
        if let Some(payload) = phase.payload {
            config.payload = payload;
        }

        for phase in scenario.compile(cli.phase.as_deref())? {
            let phase_payload = phase.payload.as_ref().unwrap_or(&config.payload);
            let action_payloads = phase.actions.iter().filter_map(|action| match action {
                ScenarioAction::Send { payload, .. } => {
                    Some(payload.as_ref().unwrap_or(phase_payload))
                }
                _ => None,
            });
            validate_reply_match(
                &config.reply_match,
                config.mode,
                std::iter::once(phase_payload).chain(action_payloads),
            )?;
        }

        Ok(config)
    }
}

/// Validates the authentication flags and returns the configured method.
#[cfg(feature = "auth")]
fn auth_from_cli(cli: &Cli) -> Result<Option<AuthConfig>> {
    use crate::cli::AuthType;
    let invalid = |message: &str| WsBlastError::from(ConfigError::InvalidAuth(message.to_string()));
    let (scheme, header, value, flag) = match cli.auth_type {
        AuthType::None => {
            if cli.auth_token.is_some() || cli.auth_apikey.is_some() || cli.auth_token_cmd.is_some()
            {
                return Err(invalid(
                    "authentication options require an explicit --auth-type",
                ));
            }
            return Ok(None);
        }
        AuthType::Static => (
            AuthScheme::Static,
            &cli.auth_header,
            &cli.auth_token,
            "--auth-token",
        ),
        AuthType::ApiKey => (
            AuthScheme::ApiKey,
            &cli.auth_apikey_header,
            &cli.auth_apikey,
            "--auth-apikey",
        ),
    };

    let credential = match &cli.auth_token_cmd {
        Some(command) => {
            if command.trim().is_empty() {
                return Err(invalid("--auth-token-cmd requires a non-empty command"));
            }
            let timeout = cli
                .auth_token_cmd_timeout
                .as_deref()
                .map_or(Ok(DEFAULT_TOKEN_CMD_TIMEOUT), parse_human_duration)?;
            if timeout.is_zero() {
                return Err(invalid(
                    "--auth-token-cmd-timeout must be greater than zero",
                ));
            }
            let refresh = cli
                .auth_token_refresh
                .as_deref()
                .map(parse_human_duration)
                .transpose()?;
            if refresh.is_some_and(|refresh| refresh.is_zero()) {
                return Err(invalid("--auth-token-refresh must be greater than zero"));
            }
            Credential::Command(TokenCommand {
                command: command.clone(),
                timeout,
                refresh,
            })
        }
        None => {
            // clap does not enforce `requires` when --auth-token or
            // --auth-apikey, which conflict with --auth-token-cmd, are given.
            for (flag, set) in [
                (
                    "--auth-token-cmd-timeout",
                    cli.auth_token_cmd_timeout.is_some(),
                ),
                ("--auth-token-refresh", cli.auth_token_refresh.is_some()),
            ] {
                if set {
                    return Err(invalid(&format!("{flag} requires --auth-token-cmd")));
                }
            }
            let value = value
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    let kind = match scheme {
                        AuthScheme::Static => "static",
                        AuthScheme::ApiKey => "apikey",
                    };
                    invalid(&format!(
                        "--auth-type {kind} requires a non-empty {flag} or --auth-token-cmd"
                    ))
                })?;
            Credential::Value(Secret::new(value.to_string()))
        }
    };

    Ok(Some(AuthConfig {
        scheme,
        header: header.clone(),
        credential,
    }))
}

/// Parses concise human duration strings into standard [`Duration`].
///
/// Supported suffixes: `ns` (nanoseconds), `us`/`µs` (microseconds),
/// `ms` (milliseconds), `s` (seconds), `m` (minutes), `h` (hours). Every
/// suffix accepts integer or fractional values (`1.5ms`, `0.5us`). A bare
/// value without a suffix is an integer number of seconds.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidDuration`] if the string is empty, is not a
/// number with a supported suffix, or is negative, NaN, infinite, or too large
/// to represent as a [`Duration`].
pub fn parse_human_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::InvalidDuration("empty duration string".into()).into());
    }

    const UNITS: [(&str, u64); 7] = [
        ("ns", 1),
        ("us", 1_000),
        ("µs", 1_000),
        ("ms", 1_000_000),
        ("s", 1_000_000_000),
        ("m", 60_000_000_000),
        ("h", 3_600_000_000_000),
    ];

    let invalid = || WsBlastError::Config(ConfigError::InvalidDuration(s.to_string()));

    for (suffix, nanos_per_unit) in UNITS {
        if let Some(value) = s.strip_suffix(suffix) {
            return scale_duration(value, nanos_per_unit).ok_or_else(invalid);
        }
    }

    s.parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| invalid())
}

fn scale_duration(value: &str, nanos_per_unit: u64) -> Option<Duration> {
    const NANOS_PER_SEC: u128 = 1_000_000_000;

    if let Ok(whole) = value.parse::<u64>() {
        let nanos = u128::from(whole) * u128::from(nanos_per_unit);
        let secs = u64::try_from(nanos / NANOS_PER_SEC).ok()?;
        let subsec = u32::try_from(nanos % NANOS_PER_SEC).ok()?;
        return Some(Duration::new(secs, subsec));
    }

    let num: f64 = value.parse().ok()?;
    if num.is_sign_negative() {
        return None;
    }
    Duration::try_from_secs_f64(num * nanos_per_unit as f64 / NANOS_PER_SEC as f64).ok()
}

/// Parses a header string into (HeaderName, HeaderValue).
pub(crate) fn parse_header_pair(header: &str) -> Result<(HeaderName, HeaderValue)> {
    let parts: Vec<&str> = header.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(ConfigError::InvalidHeader(header.to_string()).into());
    }

    let name = HeaderName::from_str(parts[0].trim())
        .map_err(|_| ConfigError::InvalidHeader(format!("Invalid header name '{}'", parts[0])))?;

    let value = HeaderValue::from_str(parts[1].trim())
        .map_err(|_| ConfigError::InvalidHeader(format!("Invalid header value '{}'", parts[1])))?;

    Ok((name, value))
}

#[cfg(test)]
mod tests {

    #[test]
    fn stage_lists_parse_and_reject_malformed_stages() {
        let stages = Stage::parse_list("30s:100, 2m:100,30s:0").unwrap();
        assert_eq!(
            stages,
            [
                Stage::new(Duration::from_secs(30), 100),
                Stage::new(Duration::from_secs(120), 100),
                Stage::new(Duration::from_secs(30), 0),
            ]
        );
        for bad in ["", "30s", "30s:many", "soon:10", ",,"] {
            assert!(Stage::parse_list(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn stage_targets_move_linearly_between_stages() {
        let stages = Stage::parse_list("10s:100,10s:100,10s:20").unwrap();
        let at = |secs: f64| Stage::target_at(&stages, Duration::from_secs_f64(secs));
        assert_eq!(at(0.0), 0);
        assert_eq!(at(2.5), 25);
        assert_eq!(at(10.0), 100);
        assert_eq!(at(15.0), 100);
        assert_eq!(at(25.0), 60);
        assert_eq!(at(30.0), 20);
        assert_eq!(at(99.0), 20);
    }

    #[test]
    fn stages_set_connections_and_duration_and_are_validated() {
        let target = "ws://127.0.0.1:9001";
        let config = LoadTestConfig::builder(target)
            .connections(5)
            .duration(Duration::from_secs(1))
            .stages(Stage::parse_list("1s:10,2s:40,1s:0").unwrap())
            .build()
            .unwrap();
        assert_eq!(config.connections(), 40);
        assert_eq!(config.duration(), Duration::from_secs(4));
        assert_eq!(config.stages().len(), 3);

        let error = |stages: &str, ramp: u64| {
            LoadTestConfig::builder(target)
                .ramp_rate(ramp)
                .stages(Stage::parse_list(stages).unwrap())
                .build()
                .unwrap_err()
                .to_string()
        };
        assert!(error("1s:10", 5).contains("--ramp-rate"));
        assert!(error("1s:10,0s:5", 0).contains("stage 2 has a zero duration"));
        assert!(error("1s:0,1s:0", 0).contains("greater than zero"));
    }
    use super::*;

    #[test]
    fn test_parse_human_durations() {
        assert_eq!(
            parse_human_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_human_duration("10s").unwrap(),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_human_duration("2m").unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_human_duration("100us").unwrap(),
            Duration::from_micros(100)
        );
        assert_eq!(parse_human_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_human_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_human_duration("1.5s").unwrap(),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn durations_accept_nanoseconds_and_fractional_units() {
        let cases = [
            ("250ns", Duration::from_nanos(250)),
            ("1.5ms", Duration::from_micros(1500)),
            ("0.5us", Duration::from_nanos(500)),
            ("0.5µs", Duration::from_nanos(500)),
            ("1.5m", Duration::from_secs(90)),
            ("0.5h", Duration::from_secs(1800)),
            ("2h", Duration::from_secs(7200)),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_human_duration(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn durations_reject_negative_non_finite_and_overflowing_values() {
        let cases = [
            "-1s",
            "-1",
            "-0.5ms",
            "-3m",
            "infs",
            "inf",
            "-infh",
            "NaNs",
            "nanms",
            "1e400s",
            "1e300h",
            "18446744073709551615h",
            "99999999999999999999",
            "s",
            "",
            "abc",
        ];
        for input in cases {
            assert!(
                matches!(
                    parse_human_duration(input),
                    Err(WsBlastError::Config(ConfigError::InvalidDuration(_)))
                ),
                "{input:?} should be rejected"
            );
        }
    }

    proptest::proptest! {
        #[test]
        fn parse_human_duration_never_panics(input in "\\PC*") {
            let _ = parse_human_duration(&input);
        }

        #[test]
        fn parse_human_duration_never_panics_on_numeric_shapes(
            input in "[-+]?([0-9]{0,25}(\\.[0-9]{0,12})?([eE][-+]?[0-9]{1,4})?|inf|NaN|infinity)(ns|us|µs|ms|s|m|h)?"
        ) {
            let _ = parse_human_duration(&input);
        }
    }

    #[test]
    fn text_payloads_are_static_unless_they_carry_placeholders() {
        let static_p = PayloadConfig::from_text("plain text without macros".to_string());
        assert!(matches!(static_p, PayloadConfig::StaticText(_)));

        let dynamic_p = PayloadConfig::from_text(r#"{"seq": {{seq}}}"#.to_string());
        assert!(matches!(dynamic_p, PayloadConfig::DynamicText(_)));
    }

    #[test]
    fn test_parse_header_pair() {
        let (name, value) = parse_header_pair("Authorization: Bearer mytoken123").unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer mytoken123");
    }

    #[test]
    fn repeated_header_flags_keep_every_value() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "-H",
            "X-Test: a",
            "-H",
            "X-Test: b",
        ]);
        let config = LoadTestConfig::from_cli(cli).unwrap();
        let values: Vec<_> = config
            .headers
            .get_all("x-test")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, ["a", "b"]);
    }

    #[test]
    fn latency_slo_outside_echo_mode_is_rejected() {
        use clap::Parser;

        for mode in ["stream", "listen"] {
            let cli = Cli::parse_from([
                "wsblast",
                "ws://127.0.0.1:9001",
                "--mode",
                mode,
                "--max-p99",
                "1us",
            ]);
            let error = LoadTestConfig::from_cli(cli).unwrap_err();
            assert!(
                error.to_string().contains("require --mode echo"),
                "{mode}: {error}"
            );
        }
    }

    #[test]
    fn non_latency_slos_are_allowed_outside_echo_mode() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--mode",
            "stream",
            "--max-error-rate",
            "0.01",
            "--min-throughput",
            "10",
        ]);
        assert!(LoadTestConfig::from_cli(cli).is_ok());
    }

    #[cfg(feature = "auth")]
    #[test]
    fn static_auth_without_token_is_rejected() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--auth-type", "static"]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("--auth-token"));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn apikey_auth_without_key_is_rejected() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--auth-type", "apikey"]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("--auth-apikey"));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn token_command_is_parsed_with_its_timing() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--auth-type",
            "apikey",
            "--auth-token-cmd",
            "mint-token --audience ws",
            "--auth-token-cmd-timeout",
            "5s",
            "--auth-token-refresh",
            "10m",
        ]);
        let auth = LoadTestConfig::from_cli(cli).unwrap().auth.unwrap();
        assert_eq!(auth.scheme, AuthScheme::ApiKey);
        assert_eq!(auth.header, "X-API-Key");
        let Credential::Command(command) = auth.credential else {
            panic!("expected a command credential");
        };
        assert_eq!(command.command, "mint-token --audience ws");
        assert_eq!(command.timeout, Duration::from_secs(5));
        assert_eq!(command.refresh, Some(Duration::from_secs(600)));

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--auth-type",
            "static",
            "--auth-token-cmd",
            "mint-token",
        ]);
        let Credential::Command(command) = LoadTestConfig::from_cli(cli)
            .unwrap()
            .auth
            .unwrap()
            .credential
        else {
            panic!("expected a command credential");
        };
        assert_eq!(command.timeout, Duration::from_secs(30));
        assert_eq!(command.refresh, None);
    }

    #[cfg(feature = "auth")]
    #[test]
    fn token_command_flag_combinations_are_checked() {
        use clap::Parser;

        let parse = |extra: &str| {
            let base = ["wsblast", "ws://127.0.0.1:9001"];
            Cli::try_parse_from(base.into_iter().chain(extra.split(' ')))
        };
        let rejected = |extra: &str| {
            LoadTestConfig::from_cli(parse(extra).unwrap())
                .unwrap_err()
                .to_string()
        };

        // Rejected by the parser.
        for extra in [
            "--auth-type static --auth-token t --auth-token-cmd c",
            "--auth-type apikey --auth-apikey k --auth-token-cmd c",
            "--auth-token-refresh 1m",
            "--auth-token-cmd-timeout 1s",
        ] {
            assert!(parse(extra).is_err(), "{extra}");
        }

        // Rejected by validation.
        for (extra, message) in [
            ("--auth-token-cmd c", "explicit --auth-type"),
            (
                "--auth-type static --auth-token t --auth-token-refresh 1m",
                "--auth-token-refresh requires --auth-token-cmd",
            ),
            (
                "--auth-type apikey --auth-apikey k --auth-token-cmd-timeout 1s",
                "--auth-token-cmd-timeout requires --auth-token-cmd",
            ),
            (
                "--auth-type static --auth-token-cmd c --auth-token-cmd-timeout 0s",
                "--auth-token-cmd-timeout must be greater than zero",
            ),
            (
                "--auth-type static --auth-token-cmd c --auth-token-refresh 0s",
                "--auth-token-refresh must be greater than zero",
            ),
            (
                "--auth-type static --auth-token-cmd c --auth-token-refresh soon",
                "Invalid duration",
            ),
        ] {
            let error = rejected(extra);
            assert!(error.contains(message), "{extra}: {error}");
        }
        let error = LoadTestConfig::from_cli(
            Cli::try_parse_from([
                "wsblast",
                "ws://127.0.0.1:9001",
                "--auth-type",
                "static",
                "--auth-token-cmd",
                " ",
            ])
            .unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-empty command"));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn auth_config_debug_output_hides_the_token() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--auth-type",
            "static",
            "--auth-token",
            "token-123",
        ]);
        let config = LoadTestConfig::from_cli(cli).unwrap();
        assert!(!format!("{config:?}").contains("token-123"));
    }

    #[cfg(feature = "graphql-ws")]
    #[test]
    fn graphql_mode_requires_an_operation() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--mode", "graphql-ws"]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("--graphql-ws-operation"));
    }

    #[cfg(feature = "graphql-ws")]
    #[test]
    fn graphql_options_require_graphql_mode() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--graphql-ws-operation",
            "subscription { ping }",
        ]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("--mode graphql-ws"));
    }

    #[cfg(feature = "graphql-ws")]
    #[test]
    fn malformed_graphql_variables_are_rejected() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--mode",
            "graphql-ws",
            "--graphql-ws-operation",
            "subscription { ping }",
            "--graphql-ws-variables",
            "{not-json}",
        ]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("valid JSON"));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_the_interactive_tui() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "--scenario", "scenario.yaml", "--tui"]);
        let error = LoadTestConfig::from_scenario_config(&scenario(vec![phase("phase")]), &cli)
            .unwrap_err();
        assert!(error.to_string().contains("TUI"));
    }

    #[cfg(feature = "scenario")]
    fn phase(name: &str) -> PhaseConfig {
        PhaseConfig {
            name: name.to_string(),
            target: None,
            duration: Some("1s".to_string()),
            connections: Some(2),
            ramp_rate: None,
            stages: None,
            rate_per_conn: 0,
            payload: None,
            think_time: None,
            actions: Vec::new(),
            slo: None,
        }
    }

    #[cfg(feature = "scenario")]
    fn scenario(phases: Vec<PhaseConfig>) -> ScenarioConfig {
        ScenarioConfig {
            version: "1.0".to_string(),
            target: Some("ws://127.0.0.1:9001".to_string()),
            metadata: ScenarioMeta {
                name: "test".to_string(),
                description: None,
                tags: None,
            },
            slo: None,
            phases,
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_compiles_durations_payloads_and_actions() {
        let mut warmup = phase("warmup");
        warmup.think_time = Some("25ms".to_string());
        warmup.payload = Some(r#"{"seq":{{seq}}}"#.to_string());
        warmup.actions = vec![
            ActionConfig::Send {
                payload: None,
                count: Some(3),
            },
            ActionConfig::Wait {
                duration: "10ms".to_string(),
            },
            ActionConfig::Ping {
                interval: "5s".to_string(),
            },
            ActionConfig::Close {
                code: Some(1001),
                reason: Some("done".to_string()),
            },
        ];

        let compiled = scenario(vec![warmup]).compile(None).unwrap();
        assert_eq!(compiled.len(), 1);

        let warmup = &compiled[0];
        assert_eq!(warmup.connections, 2);
        assert_eq!(warmup.think_time, Duration::from_millis(25));
        assert!(matches!(
            warmup.payload,
            Some(PayloadConfig::DynamicText(_))
        ));
        assert_eq!(warmup.actions.len(), 4);
        assert!(matches!(
            warmup.actions[0],
            ScenarioAction::Send { count: 3, .. }
        ));
        assert!(matches!(
            warmup.actions[1],
            ScenarioAction::Wait(duration) if duration == Duration::from_millis(10)
        ));
        assert!(matches!(
            warmup.actions[2],
            ScenarioAction::Ping(interval) if interval == Duration::from_secs(5)
        ));
        assert!(matches!(
            warmup.actions[3],
            ScenarioAction::Close { code: 1001, .. }
        ));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_duplicate_phase_names() {
        let error = scenario(vec![phase("warmup"), phase("warmup")])
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("duplicate phase name"));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_invalid_close_code() {
        let mut warmup = phase("warmup");
        warmup.actions = vec![ActionConfig::Close {
            code: Some(1005),
            reason: None,
        }];
        let error = scenario(vec![warmup]).validate().unwrap_err();
        assert!(error.to_string().contains("close code 1005"));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_zero_send_count() {
        let mut warmup = phase("warmup");
        warmup.actions = vec![ActionConfig::Send {
            payload: None,
            count: Some(0),
        }];
        let error = scenario(vec![warmup]).validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("count must be greater than zero")
        );
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_unknown_selected_phase() {
        let error = scenario(vec![phase("warmup")])
            .compile(Some("missing"))
            .unwrap_err();
        assert!(error.to_string().contains("phase 'missing' not found"));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_close_codes_follow_rfc_6455_ranges() {
        let accepted = [
            1000, 1001, 1002, 1003, 1007, 1010, 1014, 3000, 3999, 4000, 4999,
        ];
        let rejected = [
            0, 999, 1004, 1005, 1006, 1015, 1016, 1100, 2000, 2999, 5000, 65535,
        ];
        for code in accepted {
            assert_eq!(validate_close_code(code).ok(), Some(code), "{code}");
        }
        for code in rejected {
            let error = validate_close_code(code).unwrap_err().to_string();
            assert!(error.contains(&format!("close code {code}")), "{error}");
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_yaml_rejects_unknown_keys_by_name() {
        let valid = r#"
version: "1.0"
target: "ws://127.0.0.1:9001"
metadata:
  name: "strict"
  description: "text"
  tags: ["a"]
phases:
  - name: "load"
    duration: "1s"
    connections: 2
    actions:
      - type: Send
        count: 2
      - type: Wait
        duration: "10ms"
      - type: Ping
        interval: "10ms"
      - type: Close
        code: 1000
        reason: "done"
"#;
        serde_norway::from_str::<ScenarioConfig>(valid).expect("valid scenario");

        for (typo, fixed) in [
            ("tagret:", "target:"),
            ("descripton:", "description:"),
            ("conections:", "connections:"),
            ("cout:", "count:"),
            ("durration: \"10ms\"", "duration: \"10ms\""),
            ("intervall:", "interval:"),
            ("reson:", "reason:"),
        ] {
            let yaml = valid.replacen(fixed, typo, 1);
            let error = serde_norway::from_str::<ScenarioConfig>(&yaml)
                .expect_err(typo)
                .to_string();
            let key = typo.split(':').next().unwrap_or(typo);
            assert!(
                error.contains(&format!("unknown field `{key}`")),
                "{typo}: {error}"
            );
        }
    }

    #[cfg(feature = "scenario")]
    fn targeted_phase(name: &str, target: Option<&str>) -> PhaseConfig {
        PhaseConfig {
            target: target.map(ToOwned::to_owned),
            ..phase(name)
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_phase_without_any_target_is_rejected_by_name() {
        use clap::Parser;

        let mut scenario = scenario(vec![
            targeted_phase("first", Some("ws://127.0.0.1:9001")),
            targeted_phase("second", None),
        ]);
        scenario.target = None;

        let cli = Cli::parse_from(["wsblast", "--scenario", "scenario.yaml"]);
        let error = LoadTestConfig::from_scenario_config(&scenario, &cli).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("phase 'second'"), "{message}");
        assert!(message.contains("no target URL"), "{message}");

        let cli = Cli::parse_from(["wsblast", "--scenario", "scenario.yaml", "ws://10.0.0.1/ws"]);
        assert!(LoadTestConfig::from_scenario_config(&scenario, &cli).is_ok());
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_targets_prefer_cli_then_phase_then_scenario_for_every_phase() {
        let mut scenario = scenario(vec![
            targeted_phase("own", Some("ws://phase.test/a")),
            targeted_phase("shared", None),
            targeted_phase("own-again", Some("ws://phase.test/c")),
        ]);
        scenario.target = Some("ws://scenario.test/".to_string());

        let targets: Vec<String> = scenario
            .resolve_phase_targets(None, None)
            .unwrap()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            targets,
            [
                "ws://phase.test/a",
                "ws://scenario.test/",
                "ws://phase.test/c"
            ]
        );

        let targets = scenario
            .resolve_phase_targets(Some("wss://cli.test/ws"), None)
            .unwrap();
        assert_eq!(targets.len(), 3);
        assert!(
            targets
                .iter()
                .all(|url| url.as_str() == "wss://cli.test/ws")
        );

        let targets = scenario
            .resolve_phase_targets(None, Some("own-again"))
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].as_str(), "ws://phase.test/c");
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_rejects_unsupported_target_scheme() {
        let mut scenario = scenario(vec![phase("warmup")]);
        scenario.target = Some("http://127.0.0.1:9001".to_string());
        let error = scenario.validate().unwrap_err();
        assert!(error.to_string().contains("Unsupported WebSocket scheme"));
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_parses_yaml_with_target_and_actions() {
        let yaml = r#"
version: "1.0"
target: "wss://example.test/ws"
metadata:
  name: "YAML scenario"
phases:
  - name: "connect"
    duration: "2s"
    connections: 4
    actions:
      - type: Send
        count: 2
      - type: Wait
        duration: "50ms"
      - type: Close
        code: 1000
"#;
        let scenario: ScenarioConfig = serde_norway::from_str(yaml).unwrap();
        scenario.validate().unwrap();

        let compiled = scenario.compile(None).unwrap();
        assert_eq!(compiled[0].target, None);
        assert_eq!(compiled[0].actions.len(), 3);
        assert_eq!(scenario.target.as_deref(), Some("wss://example.test/ws"));
    }

    #[cfg(feature = "graphql-ws")]
    #[test]
    fn graphql_messages_use_protocol_wire_names() {
        let init =
            serde_json::to_value(GraphQLWSMessage::ConnectionInit { payload: None }).unwrap();
        assert_eq!(init["type"], "connection_init");

        let ack: GraphQLWSMessage =
            serde_json::from_value(serde_json::json!({"type": "connection_ack"})).unwrap();
        assert!(matches!(ack, GraphQLWSMessage::ConnectionAck));

        let next: GraphQLWSMessage = serde_json::from_value(serde_json::json!({
            "type": "next",
            "id": "1",
            "payload": {"data": {"price": 42}}
        }))
        .unwrap();
        assert!(matches!(next, GraphQLWSMessage::Next { .. }));

        let subscribe = serde_json::to_value(GraphQLWSMessage::Subscribe {
            id: "1".to_string(),
            payload: GraphQLWSStartPayload {
                query: "subscription { price }".to_string(),
                operation_name: None,
                variables: None,
                extensions: None,
            },
        })
        .unwrap();
        assert_eq!(subscribe["type"], "subscribe");
    }

    fn match_config(args: &[&str]) -> Result<LoadTestConfig> {
        use clap::Parser;

        let mut argv = vec!["wsblast", "ws://127.0.0.1:9001"];
        argv.extend_from_slice(args);
        LoadTestConfig::from_cli(Cli::parse_from(argv))
    }

    fn assert_unsupported_option(result: Result<LoadTestConfig>, needle: &str) {
        match result {
            Err(WsBlastError::Config(ConfigError::UnsupportedOption(message))) => {
                assert!(message.contains(needle), "{message}");
            }
            other => panic!("expected UnsupportedOption containing '{needle}', got {other:?}"),
        }
    }

    #[test]
    fn reply_match_defaults_to_first() {
        let config = match_config(&[]).unwrap();
        assert_eq!(config.reply_match, ReplyMatch::First);
    }

    #[test]
    fn reply_match_parses_every_mode() {
        let exact = match_config(&["--match", "exact", "--payload", "ping"]).unwrap();
        assert_eq!(exact.reply_match, ReplyMatch::Exact);

        let json = match_config(&["--match", "json:/data/seq"]).unwrap();
        assert_eq!(
            json.reply_match,
            ReplyMatch::JsonPointer("/data/seq".to_string())
        );

        let escaped = match_config(&["--match", "json:/a~1b/~0c"]).unwrap();
        assert_eq!(
            escaped.reply_match,
            ReplyMatch::JsonPointer("/a~1b/~0c".to_string())
        );
    }

    #[test]
    fn unknown_reply_match_is_rejected() {
        assert_unsupported_option(match_config(&["--match", "last"]), "--match");
    }

    #[test]
    fn json_reply_match_requires_seq_placeholder() {
        assert_unsupported_option(
            match_config(&["--match", "json:/seq", "--payload", r#"{"seq":1}"#]),
            "{{seq}}",
        );
        assert_unsupported_option(
            match_config(&["--match", "json:/seq", "--payload", "{{timestamp}}"]),
            "{{seq}}",
        );
        assert_unsupported_option(
            match_config(&["--match", "json:/seq", "--binary", "--payload", "{{seq}}"]),
            "{{seq}}",
        );
    }

    #[test]
    fn json_reply_match_rejects_invalid_pointers() {
        for pointer in ["json:seq", "json:/seq~", "json:/a~2b"] {
            assert_unsupported_option(match_config(&["--match", pointer]), "RFC 6901");
        }
    }

    #[test]
    fn reply_match_requires_echo_mode() {
        for mode in ["stream", "listen"] {
            assert_unsupported_option(
                match_config(&["--mode", mode, "--match", "json:/seq"]),
                "--mode echo",
            );
            assert_unsupported_option(
                match_config(&["--mode", mode, "--match", "exact"]),
                "--mode echo",
            );
        }
        assert!(match_config(&["--mode", "stream", "--match", "first"]).is_ok());
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn json_reply_match_checks_scenario_payloads() {
        use clap::Parser;

        let cli = Cli::parse_from([
            "wsblast",
            "--scenario",
            "scenario.yaml",
            "--match",
            "json:/seq",
        ]);

        let mut static_phase = phase("static");
        static_phase.payload = Some("ping".to_string());
        assert_unsupported_option(
            LoadTestConfig::from_scenario_config(&scenario(vec![static_phase]), &cli),
            "{{seq}}",
        );

        let mut later_phase = phase("later");
        later_phase.actions = vec![ActionConfig::Send {
            payload: Some("ping".to_string()),
            count: None,
        }];
        assert_unsupported_option(
            LoadTestConfig::from_scenario_config(
                &scenario(vec![phase("first"), later_phase]),
                &cli,
            ),
            "{{seq}}",
        );

        let mut templated = phase("templated");
        templated.payload = Some(r#"{"seq":{{seq}}}"#.to_string());
        assert!(LoadTestConfig::from_scenario_config(&scenario(vec![templated]), &cli).is_ok());
    }

    /// A scenario with a top-level `slo:` mapping and one phase with its own.
    #[cfg(feature = "scenario")]
    fn slo_yaml(top: &str, phase: &str) -> String {
        format!(
            "version: \"1.0\"\ntarget: \"ws://127.0.0.1:9001\"\nmetadata:\n  name: \"slo\"\n\
             {top}phases:\n  - name: \"spike\"\n    duration: \"1s\"\n    connections: 2\n{phase}"
        )
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_and_phase_slos_are_parsed() {
        let yaml = slo_yaml(
            "slo:\n  max_error_rate: 0.01\n  min_throughput: 50\n",
            "    slo:\n      max_p50: \"2ms\"\n      max_p95: 5ms\n      max_p99: \"0.5s\"\n      max_p999: 1\n",
        );
        let scenario = ScenarioConfig::from_yaml(&yaml).unwrap();
        assert_eq!(
            scenario.slo().unwrap(),
            SloThresholds::new()
                .with_max_error_rate(0.01)
                .with_min_throughput(50.0)
        );
        let phases = scenario.compile(None).unwrap();
        assert_eq!(
            phases[0].slo,
            SloThresholds::new()
                .with_max_p50(Duration::from_millis(2))
                .with_max_p95(Duration::from_millis(5))
                .with_max_p99(Duration::from_millis(500))
                .with_max_p999(Duration::from_secs(1))
        );

        let plain = ScenarioConfig::from_yaml(&slo_yaml("", "")).unwrap();
        assert_eq!(plain.slo().unwrap(), SloThresholds::default());
        assert_eq!(
            plain.compile(None).unwrap()[0].slo,
            SloThresholds::default()
        );
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn unknown_slo_keys_are_rejected_by_name() {
        for yaml in [
            slo_yaml("slo:\n  max_p99_ms: 5\n", ""),
            slo_yaml("", "    slo:\n      max_p99_ms: 5\n"),
        ] {
            let message = ScenarioConfig::from_yaml(&yaml).unwrap_err().to_string();
            assert!(message.contains("unknown field `max_p99_ms`"), "{message}");
            assert!(message.contains("`max_p999`"), "{message}");
        }
        let message = ScenarioConfig::from_yaml(&slo_yaml("", "    sl0:\n      max_p99: 5ms\n"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("unknown field `sl0`"), "{message}");
        assert!(message.contains("`slo`"), "{message}");
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn invalid_slo_values_are_rejected_with_the_flag_messages() {
        for (yaml, expected) in [
            (
                slo_yaml("", "    slo:\n      max_p99: \"fast\"\n"),
                "phase 'spike': slo: max_p99: Configuration error: Invalid duration 'fast'",
            ),
            (
                slo_yaml("", "    slo:\n      max_error_rate: 1.5\n"),
                "phase 'spike': slo: Configuration error: Invalid threshold specification: \
                 max_error_rate must be between 0.0 and 1.0, received 1.5",
            ),
            (
                slo_yaml("slo:\n  max_error_rate: -0.1\n", ""),
                "slo: Configuration error: Invalid threshold specification: \
                 max_error_rate must be between 0.0 and 1.0, received -0.1",
            ),
            (
                slo_yaml("slo:\n  max_p50: \"-1ms\"\n", ""),
                "slo: max_p50: Configuration error: Invalid duration '-1ms'",
            ),
        ] {
            let error = ScenarioConfig::from_yaml(&yaml).unwrap_err();
            assert!(
                matches!(
                    &error,
                    WsBlastError::Config(ConfigError::InvalidScenario(_))
                ),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(message.contains(expected), "{message}");
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn latency_slos_need_echo_mode_in_every_phase_that_runs() {
        use clap::Parser;

        let phase_latency = ScenarioConfig::from_yaml(&slo_yaml(
            "",
            "    slo:\n      max_p99: 5ms\n      max_error_rate: 0.1\n",
        ))
        .unwrap();
        let stream = Cli::parse_from(["wsblast", "--scenario", "s.yaml", "--mode", "stream"]);
        let error = LoadTestConfig::from_scenario_config(&phase_latency, &stream).unwrap_err();
        assert!(
            matches!(
                &error,
                WsBlastError::Config(ConfigError::UnsupportedOption(_))
            ),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(
                "phase 'spike': slo: latency SLOs (max_p50/max_p95/max_p99/max_p999) require \
                 --mode echo; Stream mode does not measure round-trip time"
            ),
            "{message}"
        );
        let echo = Cli::parse_from(["wsblast", "--scenario", "s.yaml"]);
        assert!(LoadTestConfig::from_scenario_config(&phase_latency, &echo).is_ok());

        let top_latency =
            ScenarioConfig::from_yaml(&slo_yaml("slo:\n  max_p50: 5ms\n", "")).unwrap();
        let message = LoadTestConfig::from_scenario_config(&top_latency, &stream)
            .unwrap_err()
            .to_string();
        assert!(message.contains("slo: latency SLOs (max_p50/"), "{message}");

        // The library entry point applies the same rule to its base mode.
        let base = LoadTestConfig::builder("ws://127.0.0.1:9001")
            .mode(TestMode::Stream)
            .build()
            .unwrap();
        let message = crate::ScenarioExecutor::new(&phase_latency, base, None, None)
            .err()
            .expect("latency SLO in stream mode")
            .to_string();
        assert!(
            message.contains("phase 'spike': slo: latency SLOs"),
            "{message}"
        );

        // Error and throughput budgets need no round trips.
        let budgets = ScenarioConfig::from_yaml(&slo_yaml(
            "slo:\n  min_throughput: 1\n",
            "    slo:\n      max_error_rate: 0.1\n",
        ))
        .unwrap();
        assert!(LoadTestConfig::from_scenario_config(&budgets, &stream).is_ok());
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn command_line_slos_override_the_scenario_threshold_by_threshold() {
        let scenario = SloThresholds::new()
            .with_max_p99(Duration::from_millis(50))
            .with_max_error_rate(0.05);
        let cli = SloThresholds::new()
            .with_max_p99(Duration::from_millis(10))
            .with_min_throughput(100.0);
        assert_eq!(
            cli.or_from(&scenario),
            SloThresholds::new()
                .with_max_p99(Duration::from_millis(10))
                .with_max_error_rate(0.05)
                .with_min_throughput(100.0)
        );
        assert_eq!(SloThresholds::new().or_from(&scenario), scenario);
        assert_eq!(cli.or_from(&SloThresholds::new()), cli);
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn example_scenarios_are_valid() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/scenarios");
        let mut loaded = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
            {
                let scenario = ScenarioConfig::load(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                scenario.compile(None).unwrap();
                loaded += 1;
            }
        }
        assert!(loaded > 0, "no scenarios in {}", dir.display());
    }

    /// A scenario with one phase `ramp` whose fields after `name` are
    /// `fields`, each line indented by four spaces.
    #[cfg(feature = "scenario")]
    fn staged_yaml(fields: &str) -> String {
        format!(
            "version: \"1.0\"\ntarget: \"ws://127.0.0.1:9001\"\nmetadata:\n  name: \"staged\"\n\
             phases:\n  - name: \"ramp\"\n{fields}"
        )
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn phase_stages_parse_from_a_list_or_the_flag_syntax() {
        let list = staged_yaml(
            "    rate_per_conn: 5\n    stages:\n      - duration: \"30s\"\n        target: 100\n\
             \x20     - { duration: 1m, target: 100 }\n      - duration: \"500ms\"\n        target: 0\n",
        );
        let spec = staged_yaml("    rate_per_conn: 5\n    stages: \"30s:100, 1m:100,500ms:0\"\n");
        let expected = vec![
            Stage::new(Duration::from_secs(30), 100),
            Stage::new(Duration::from_secs(60), 100),
            Stage::new(Duration::from_millis(500), 0),
        ];
        for yaml in [list, spec] {
            let phase = ScenarioConfig::from_yaml(&yaml)
                .unwrap()
                .compile(None)
                .unwrap();
            let phase = &phase[0];
            assert_eq!(phase.stages, expected, "{yaml}");
            assert_eq!(phase.connections, 100);
            assert_eq!(phase.duration, Duration::from_millis(90_500));
            assert_eq!(phase.ramp_rate, 0);
            assert_eq!(phase.rate_per_conn, 5);
        }

        let constant = ScenarioConfig::from_yaml(&staged_yaml(
            "    duration: \"2s\"\n    connections: 3\n    ramp_rate: 1\n",
        ))
        .unwrap()
        .compile(None)
        .unwrap();
        assert!(constant[0].stages.is_empty());
        assert_eq!(constant[0].ramp_rate, 1);
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn phase_stages_cannot_be_mixed_with_connections_duration_or_ramp_rate() {
        for (extra, keys) in [
            ("    connections: 5\n", "`connections`"),
            ("    duration: \"10s\"\n", "`duration`"),
            ("    ramp_rate: 2\n", "`ramp_rate`"),
            (
                "    connections: 5\n    duration: \"10s\"\n",
                "`connections`, `duration`",
            ),
        ] {
            let yaml = staged_yaml(&format!("{extra}    stages: \"10s:5\"\n"));
            let message = ScenarioConfig::from_yaml(&yaml).unwrap_err().to_string();
            assert!(
                message.contains(&format!(
                    "phase 'ramp': `stages` cannot be combined with {keys};"
                )),
                "{message}"
            );
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn invalid_phase_stages_and_missing_sizes_are_rejected_by_phase() {
        for (fields, expected) in [
            (
                "    duration: \"1s\"\n",
                "phase 'ramp': missing field `connections`; give `connections` and `duration`, or `stages`",
            ),
            (
                "    connections: 2\n",
                "phase 'ramp': missing field `duration`",
            ),
            (
                "    stages: []\n",
                "phase 'ramp': stages must list at least one stage",
            ),
            (
                "    stages: \" , \"\n",
                "phase 'ramp': stages must list at least one stage",
            ),
            (
                "    stages: \"10s:5,0s:5\"\n",
                "phase 'ramp': stage 2 has a zero duration",
            ),
            (
                "    stages: \"10s:0,5s:0\"\n",
                "phase 'ramp': every stage target is zero",
            ),
            (
                "    stages: \"10s=5\"\n",
                "phase 'ramp': stages: Configuration error: Invalid option: invalid stage '10s=5'",
            ),
            (
                "    stages:\n      - duration: \"soon\"\n        target: 5\n",
                "phase 'ramp': stage 1: duration: Configuration error: Invalid duration 'soon'",
            ),
            (
                "    stages:\n      - duration: \"1s\"\n        targets: 5\n",
                "phases[0].stages[0]: unknown field `targets`, expected `duration` or `target`",
            ),
            (
                "    stages:\n      - duration: \"1s\"\n",
                "missing field `target`",
            ),
            (
                "    stages: 5\n",
                "a list of {duration, target} stages or a string",
            ),
        ] {
            let message = ScenarioConfig::from_yaml(&staged_yaml(fields))
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{fields}: {message}");
        }
    }
}
