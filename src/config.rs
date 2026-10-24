//! Validated runtime configuration model for load test execution.

use crate::cli::{Cli, OutputFormat, TestMode};
use crate::error::{ConfigError, Result, WsBlastError};
use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
#[cfg(any(feature = "auth", feature = "scenario", feature = "graphql-ws"))]
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

/// Fully validated, immutable configuration for load test execution.
#[derive(Debug, Clone)]
pub struct LoadTestConfig {
    pub target_url: Url,
    pub connections: usize,
    pub ramp_rate: u64,
    pub duration: Duration,
    pub max_requests: Option<u64>,
    pub rate_per_conn: u64,
    pub payload: PayloadConfig,
    pub headers: HeaderMap,
    pub subprotocol: Option<String>,
    pub mode: TestMode,
    pub connect_timeout: Duration,
    pub message_timeout: Duration,
    pub ping_interval: Duration,
    #[cfg(feature = "compression")]
    pub compression: CompressionConfig,
    #[cfg(feature = "auth")]
    pub auth: Option<AuthConfig>,
    #[cfg(feature = "graphql-ws")]
    pub graphql_ws: Option<GraphQLWSConfig>,
    #[cfg(feature = "capture")]
    pub capture: CaptureConfig,
    #[cfg(feature = "chaos")]
    pub chaos: ChaosConfig,
    pub slo: SloThresholds,
    pub output_format: OutputFormat,
    pub output_path: Option<PathBuf>,
    pub tui: bool,
    pub no_progress: bool,
    /// Ends a worker on its first connection loss instead of reconnecting.
    pub no_reconnect: bool,
}

/// Payload specification supporting zero-allocation cached frames or dynamic templating.
#[derive(Debug, Clone)]
pub enum PayloadConfig {
    /// Static UTF-8 text cached as Utf8Bytes for O(1) zero-allocation cloning.
    StaticText(Utf8Bytes),
    /// Dynamic text template requiring macro substitutions on each frame dispatch.
    DynamicText(String),
    /// Binary frame payload cached as Bytes for O(1) zero-allocation cloning.
    Binary(Bytes),
}

impl PayloadConfig {
    /// Classifies text payload into either a pre-allocated static frame or a dynamic template.
    pub fn from_text(text: String) -> Self {
        if text.contains("{{") {
            Self::DynamicText(text)
        } else {
            Self::StaticText(Utf8Bytes::from(text))
        }
    }
}

/// Service Level Objective thresholds for automated CI/CD pass/fail gating.
#[derive(Debug, Clone, Default)]
pub struct SloThresholds {
    pub max_p50: Option<Duration>,
    pub max_p95: Option<Duration>,
    pub max_p99: Option<Duration>,
    pub max_p999: Option<Duration>,
    pub max_error_rate: Option<f64>,
    pub min_throughput: Option<f64>,
}

#[cfg(feature = "otel")]
#[derive(Debug, Clone)]
pub struct OtelConfig {
    pub endpoint: String,
    pub service_name: String,
}

#[cfg(feature = "prometheus")]
#[derive(Debug, Clone)]
pub struct PrometheusConfig {
    pub listen_addr: String,
}

#[cfg(feature = "compression")]
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    pub enabled: bool,
    pub level: u8,
}

#[cfg(feature = "auth")]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AuthConfig {
    Static {
        token: String,
        header: String,
    },
    Jwt {
        endpoint: Url,
        client_id: String,
        secret: String,
        scopes: Vec<String>,
    },
    OAuth2 {
        token_url: Url,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
    },
    ApiKey {
        key: String,
        header: String,
    },
    Custom {
        refresh_fn: String,
    },
}

#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone)]
pub struct GraphQLWSConfig {
    pub operation_name: Option<String>,
    pub operation: String,
    pub variables: Option<serde_json::Value>,
    pub extensions: Option<serde_json::Value>,
}

#[cfg(feature = "graphql-ws")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GraphQLWSMessage {
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
pub struct GraphQLWSStartPayload {
    pub query: String,
    #[serde(rename = "operationName")]
    pub operation_name: Option<String>,
    pub variables: Option<serde_json::Value>,
    pub extensions: Option<serde_json::Value>,
}

#[cfg(feature = "capture")]
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub output_path: PathBuf,
    pub format: CaptureFormat,
    pub max_frames: usize,
}

#[cfg(feature = "capture")]
pub use crate::cli::CaptureFormat;

#[cfg(feature = "chaos")]
#[derive(Debug, Clone)]
pub struct ChaosConfig {
    pub enabled: bool,
    pub latency_ms: Option<u64>,
    pub loss_percent: Option<f64>,
    pub corrupt_percent: Option<f64>,
    pub close_percent: Option<f64>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioConfig {
    pub version: String,
    /// Optional target endpoint for every phase. CLI `--url` takes precedence.
    #[serde(default)]
    pub target: Option<String>,
    pub metadata: ScenarioMeta,
    pub phases: Vec<PhaseConfig>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioMeta {
    pub name: String,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
pub struct PhaseConfig {
    pub name: String,
    /// Optional endpoint override for this phase.
    #[serde(default)]
    pub target: Option<String>,
    pub duration: String,
    pub connections: usize,
    #[serde(default)]
    pub ramp_rate: u64,
    #[serde(default)]
    pub rate_per_conn: u64,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub think_time: Option<String>,
    #[serde(default)]
    pub actions: Vec<ActionConfig>,
}

#[cfg(feature = "scenario")]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ActionConfig {
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
pub struct ScenarioPhase {
    pub name: String,
    pub target: Option<Url>,
    pub duration: Duration,
    pub connections: usize,
    pub ramp_rate: u64,
    pub rate_per_conn: u64,
    pub payload: Option<PayloadConfig>,
    pub think_time: Duration,
    pub actions: Vec<ScenarioAction>,
}

/// Execution-ready action with parsed durations and payload templates.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone)]
pub enum ScenarioAction {
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
        let scenario: Self = serde_yaml::from_str(&content)
            .map_err(|e| ConfigError::InvalidScenario(format!("{}: {e}", path.display())))?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Validates scenario metadata, phase uniqueness, and every phase action.
    pub fn validate(&self) -> Result<()> {
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

    /// Resolves the requested phase, defaulting to the first declared phase.
    pub fn selected_phase(&self, name: Option<&str>) -> Result<&PhaseConfig> {
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

    /// Compiles every phase, or only the named phase when `selected` is set.
    pub fn compile(&self, selected: Option<&str>) -> Result<Vec<ScenarioPhase>> {
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
    /// counts, action counts, or close codes are invalid.
    pub fn compile(phase: &PhaseConfig) -> Result<Self> {
        if phase.connections == 0 {
            return Err(invalid_phase(&phase.name, "connections must be greater than zero").into());
        }

        let duration = parse_human_duration(&phase.duration)
            .map_err(|e| ConfigError::InvalidScenario(format!("phase '{}': {e}", phase.name)))?;
        if duration.is_zero() {
            return Err(invalid_phase(&phase.name, "duration must be greater than zero").into());
        }

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
            connections: phase.connections,
            ramp_rate: phase.ramp_rate,
            rate_per_conn: phase.rate_per_conn,
            payload: phase
                .payload
                .as_ref()
                .map(|payload| PayloadConfig::from_text(payload.clone())),
            think_time,
            actions,
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
    pub fn compile(phase_name: &str, action: &ActionConfig) -> Result<Self> {
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

#[cfg(feature = "chaos")]
fn validate_percentage(name: &str, value: Option<f64>) -> Result<()> {
    if let Some(value) = value {
        if !(0.0..=100.0).contains(&value) {
            return Err(ConfigError::InvalidThreshold(format!(
                "chaos {name} percentage must be between 0 and 100, received {value}"
            ))
            .into());
        }
    }
    Ok(())
}

#[cfg(feature = "scenario")]
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

#[cfg(feature = "scenario")]
fn validate_close_code(code: u16) -> Result<u16> {
    const RESERVED: [u16; 4] = [1004, 1005, 1006, 1015];
    if (1000..=4999).contains(&code) && !RESERVED.contains(&code) {
        Ok(code)
    } else {
        Err(ConfigError::InvalidScenario(format!(
            "close code {code} cannot be sent; use 1000-4999 excluding 1004, 1005, 1006, 1015"
        ))
        .into())
    }
}

#[cfg(feature = "graphql-ws")]
fn parse_graphql_json_object(flag: &str, raw: Option<&str>) -> Result<Option<serde_json::Value>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        ConfigError::InvalidThreshold(format!("{flag} must contain valid JSON: {e}"))
    })?;
    if !value.is_object() {
        return Err(
            ConfigError::InvalidThreshold(format!("{flag} must contain a JSON object")).into(),
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
            ConfigError::InvalidThreshold(
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
    pub fn from_cli(cli: Cli) -> Result<Self> {
        Self::from_cli_with_target(cli, None)
    }

    /// Builds configuration from CLI arguments, optionally overriding the
    /// target URL with a value from a scenario file.
    fn from_cli_with_target(cli: Cli, target_override: Option<String>) -> Result<Self> {
        let raw_url = target_override
            .or_else(|| cli.resolve_url().map(ToOwned::to_owned))
            .ok_or_else(|| {
                ConfigError::InvalidUrl(
                    "".into(),
                    "No target URL provided. Specify target as argument or --url <URL>".into(),
                )
            })?;

        let target_url = Url::parse(&raw_url)
            .map_err(|e| ConfigError::InvalidUrl(raw_url.clone(), e.to_string()))?;

        match target_url.scheme() {
            "ws" | "wss" => {}
            other => return Err(ConfigError::UnsupportedScheme(other.to_string()).into()),
        }

        if cli.connections == 0 {
            return Err(ConfigError::ZeroConcurrency(0).into());
        }

        #[cfg(feature = "compression")]
        if cli.compression {
            return Err(ConfigError::CompressionUnsupported.into());
        }

        #[cfg(feature = "chaos")]
        {
            validate_percentage("loss", cli.chaos_loss)?;
            validate_percentage("corruption", cli.chaos_corrupt)?;
            validate_percentage("close", cli.chaos_close)?;
        }

        if let Some(0) = cli.requests {
            return Err(ConfigError::InvalidThreshold(
                "requests limit (-n) must be greater than zero".to_string(),
            )
            .into());
        }

        let duration = parse_human_duration(&cli.duration)?;
        let connect_timeout = parse_human_duration(&cli.connect_timeout)?;
        let message_timeout = parse_human_duration(&cli.message_timeout)?;
        let ping_interval = parse_human_duration(&cli.ping_interval)?;

        #[cfg(feature = "graphql-ws")]
        let graphql_ws = build_graphql_config(&cli)?;

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
        } else if let Some(text) = cli.payload {
            if cli.binary {
                PayloadConfig::Binary(Bytes::from(text.into_bytes()))
            } else {
                PayloadConfig::from_text(text)
            }
        } else if cli.binary {
            PayloadConfig::Binary(Bytes::from_static(b"wsblast-payload"))
        } else {
            PayloadConfig::from_text(
                r#"{"source":"wsblast","ts":"{{timestamp}}","seq":{{seq}}}"#.to_string(),
            )
        };

        let mut headers = HeaderMap::new();
        for h in &cli.headers {
            let (name, value) = parse_header_pair(h)?;
            headers.insert(name, value);
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

        if let Some(err_rate) = slo.max_error_rate {
            if !(0.0..=1.0).contains(&err_rate) {
                return Err(ConfigError::InvalidThreshold(format!(
                    "max_error_rate must be between 0.0 and 1.0, received {err_rate}"
                ))
                .into());
            }
        }

        // Only echo mode measures round-trip time; any other mode would leave the
        // RTT histogram empty and a latency budget could never be evaluated.
        let has_latency_slo = slo.max_p50.is_some()
            || slo.max_p95.is_some()
            || slo.max_p99.is_some()
            || slo.max_p999.is_some();
        if has_latency_slo && cli.mode != TestMode::Echo {
            return Err(ConfigError::UnsupportedOption(format!(
                "latency SLOs (--max-p50/--max-p95/--max-p99/--max-p999) require --mode echo; {:?} mode does not measure round-trip time",
                cli.mode
            ))
            .into());
        }

        #[cfg(feature = "auth")]
        let auth = {
            use crate::cli::AuthType;
            match cli.auth_type {
                AuthType::None => {
                    if cli.auth_token.is_some()
                        || cli.auth_apikey.is_some()
                        || cli.auth_jwt_endpoint.is_some()
                        || cli.auth_oauth2_token_url.is_some()
                    {
                        return Err(ConfigError::InvalidThreshold(
                            "authentication options require an explicit --auth-type".to_string(),
                        )
                        .into());
                    }
                    None
                }
                AuthType::Static => {
                    let token = cli
                        .auth_token
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            ConfigError::InvalidThreshold(
                                "--auth-type static requires a non-empty --auth-token".to_string(),
                            )
                        })?;
                    Some(AuthConfig::Static {
                        token: token.to_string(),
                        header: cli.auth_header.clone(),
                    })
                }
                AuthType::Jwt => {
                    let endpoint = cli.auth_jwt_endpoint.as_ref().ok_or_else(|| {
                        ConfigError::InvalidThreshold(
                            "JWT authentication requires --auth-jwt-endpoint".to_string(),
                        )
                    })?;
                    let client_id = cli.auth_jwt_client_id.as_ref().ok_or_else(|| {
                        ConfigError::InvalidThreshold(
                            "JWT authentication requires --auth-jwt-client-id".to_string(),
                        )
                    })?;
                    let secret = cli.auth_jwt_secret.as_ref().ok_or_else(|| {
                        ConfigError::InvalidThreshold(
                            "JWT authentication requires --auth-jwt-secret".to_string(),
                        )
                    })?;
                    let scopes = cli.auth_jwt_scopes.clone();
                    let endpoint_url = Url::parse(endpoint).map_err(|e| {
                        ConfigError::InvalidUrl(endpoint.to_string(), e.to_string())
                    })?;
                    Some(AuthConfig::Jwt {
                        endpoint: endpoint_url,
                        client_id: client_id.clone(),
                        secret: secret.clone(),
                        scopes,
                    })
                }
                AuthType::OAuth2 => {
                    let token_url = cli.auth_oauth2_token_url.as_ref().ok_or_else(|| {
                        ConfigError::InvalidThreshold(
                            "OAuth2 authentication requires --auth-oauth2-token-url".to_string(),
                        )
                    })?;
                    let client_id = cli.auth_oauth2_client_id.as_ref().ok_or_else(|| {
                        ConfigError::InvalidThreshold(
                            "OAuth2 authentication requires --auth-oauth2-client-id".to_string(),
                        )
                    })?;
                    let client_secret =
                        cli.auth_oauth2_client_secret.as_ref().ok_or_else(|| {
                            ConfigError::InvalidThreshold(
                                "OAuth2 authentication requires --auth-oauth2-client-secret"
                                    .to_string(),
                            )
                        })?;
                    let scopes = cli.auth_oauth2_scopes.clone();
                    let token_url_parsed = Url::parse(token_url).map_err(|e| {
                        ConfigError::InvalidUrl(token_url.to_string(), e.to_string())
                    })?;
                    Some(AuthConfig::OAuth2 {
                        token_url: token_url_parsed,
                        client_id: client_id.clone(),
                        client_secret: client_secret.clone(),
                        scopes,
                    })
                }
                AuthType::ApiKey => {
                    let key = cli
                        .auth_apikey
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            ConfigError::InvalidThreshold(
                                "--auth-type apikey requires a non-empty --auth-apikey".to_string(),
                            )
                        })?;
                    Some(AuthConfig::ApiKey {
                        key: key.to_string(),
                        header: cli.auth_apikey_header.clone(),
                    })
                }
            }
        };

        Ok(Self {
            target_url,
            connections: cli.connections,
            ramp_rate: cli.ramp_rate,
            duration,
            max_requests: cli.requests,
            rate_per_conn: cli.rate,
            payload,
            headers,
            subprotocol: cli.subprotocol,
            mode: cli.mode,
            connect_timeout,
            message_timeout,
            ping_interval,
            #[cfg(feature = "compression")]
            compression: CompressionConfig {
                enabled: cli.compression,
                level: cli.compression_level.clamp(1, 9),
            },
            #[cfg(feature = "auth")]
            auth,
            #[cfg(feature = "graphql-ws")]
            graphql_ws,
            #[cfg(feature = "capture")]
            capture: CaptureConfig {
                enabled: cli.capture,
                output_path: cli
                    .capture_output
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("capture.pcapng")),
                format: cli.capture_format,
                max_frames: cli.capture_max_frames,
            },
            #[cfg(feature = "chaos")]
            chaos: ChaosConfig {
                enabled: cli.chaos
                    || cli.chaos_latency.is_some()
                    || cli.chaos_loss.is_some()
                    || cli.chaos_corrupt.is_some()
                    || cli.chaos_close.is_some(),
                latency_ms: cli.chaos_latency,
                loss_percent: cli.chaos_loss,
                corrupt_percent: cli.chaos_corrupt,
                close_percent: cli.chaos_close,
            },
            slo,
            output_format: cli.format,
            output_path: cli.output_file,
            tui: cli.tui,
            no_progress: cli.no_progress,
            no_reconnect: cli.no_reconnect,
        })
    }

    /// Builds a validated configuration from a scenario file's selected phase.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the scenario file is invalid or when CLI
    /// validation fails.
    #[cfg(feature = "scenario")]
    pub fn from_scenario(path: &Path, cli: &Cli) -> Result<Self> {
        let scenario = ScenarioConfig::load(path)?;
        Self::from_scenario_config(&scenario, cli)
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
    pub fn from_scenario_config(scenario: &ScenarioConfig, cli: &Cli) -> Result<Self> {
        if cli.tui {
            return Err(ConfigError::UnsupportedOption(
                "the interactive TUI is not supported with --scenario; use the standard progress output"
                    .to_string(),
            )
            .into());
        }

        let phase = ScenarioPhase::compile(scenario.selected_phase(cli.phase.as_deref())?)?;

        // Precedence: explicit CLI URL, then phase endpoint, then scenario endpoint.
        let target = cli
            .resolve_url()
            .map(ToOwned::to_owned)
            .or_else(|| phase.target.as_ref().map(Url::to_string))
            .or_else(|| scenario.target.clone());

        let mut config = Self::from_cli_with_target(cli.clone(), target)?;

        config.connections = phase.connections;
        config.duration = phase.duration;
        config.ramp_rate = phase.ramp_rate;
        config.rate_per_conn = phase.rate_per_conn;
        if let Some(payload) = phase.payload {
            config.payload = payload;
        }

        Ok(config)
    }
}

/// Parses concise human duration strings into standard [`Duration`].
///
/// Supported suffix multipliers: `us`/`µs` (microseconds), `ms` (milliseconds),
/// `s` (seconds), `m` (minutes), `h` (hours). Defaults to integer seconds if no suffix is present.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidDuration`] if the string is empty or contains non-numeric components.
pub fn parse_human_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::InvalidDuration("empty duration string".into()).into());
    }

    if let Some(val) = s.strip_suffix("us").or_else(|| s.strip_suffix("µs")) {
        let num: u64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_micros(num));
    }

    if let Some(val) = s.strip_suffix("ms") {
        let num: u64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_millis(num));
    }

    if let Some(val) = s.strip_suffix('s') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num));
    }

    if let Some(val) = s.strip_suffix('m') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num * 60.0));
    }

    if let Some(val) = s.strip_suffix('h') {
        let num: f64 = val
            .parse()
            .map_err(|_| ConfigError::InvalidDuration(s.to_string()))?;
        return Ok(Duration::from_secs_f64(num * 3600.0));
    }

    if let Ok(num) = s.parse::<u64>() {
        return Ok(Duration::from_secs(num));
    }

    Err(WsBlastError::Config(ConfigError::InvalidDuration(
        s.to_string(),
    )))
}

/// Parses a header string into (HeaderName, HeaderValue).
pub fn parse_header_pair(header: &str) -> Result<(HeaderName, HeaderValue)> {
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
    }

    #[test]
    fn test_parse_header_pair() {
        let (name, value) = parse_header_pair("Authorization: Bearer mytoken123").unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer mytoken123");
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

    #[cfg(feature = "compression")]
    #[test]
    fn compression_flag_is_rejected() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--compression"]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("permessage-deflate"));
    }

    #[cfg(feature = "chaos")]
    #[test]
    fn chaos_percentages_are_validated() {
        use clap::Parser;

        let cli = Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--chaos-loss", "150"]);
        let error = LoadTestConfig::from_cli(cli).unwrap_err();
        assert!(error.to_string().contains("between 0 and 100"));
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
            duration: "1s".to_string(),
            connections: 2,
            ramp_rate: 0,
            rate_per_conn: 0,
            payload: None,
            think_time: None,
            actions: Vec::new(),
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
        let scenario: ScenarioConfig = serde_yaml::from_str(yaml).unwrap();
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
}
