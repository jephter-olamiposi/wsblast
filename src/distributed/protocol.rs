//! The control protocol between a coordinator and its agents: the messages,
//! the wire forms of a configuration and of a result, how a run is split
//! between agents, and the token check.
//!
//! Every message is one JSON text frame with a `type` field. The `hello`
//! exchange carries [`PROTOCOL_VERSION`] and stays the same in every version,
//! so two builds that speak different versions always fail with a clear
//! message instead of misreading each other. docs/distributed.md describes
//! the sequence.

use crate::cli::TestMode;
use crate::config::{LoadTestConfig, PayloadConfig, ReplyMatch, Stage};
use crate::error::{ConfigError, ErrorCategory, Result, WsBlastError};
use crate::lag::LagTotals;
use crate::metrics::{ChaosSummary, LiveSnapshot, MetricsTotals, OpenFileLimit};
use crate::timeseries::{SeriesPart, TimeSeries, interval_histogram};
use clap::ValueEnum;
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// Version of the control protocol. Raised on any incompatible change to a
/// message after `hello`.
///
/// Version 2 added scenario phases (several `prepare` to `result` rounds on
/// one connection), capture records, refreshed authentication tokens and
/// interval latency in snapshots. Version 3 added chaos injection to the
/// configuration, and the agent's counts of injected faults, its generator
/// lag and its open file limit to its result.
pub const PROTOCOL_VERSION: u32 = 3;

/// The shortest agent token accepted.
pub const MIN_TOKEN_LEN: usize = 16;

/// Environment variable that holds the agent token for `wsblast agent` and
/// for a coordinator run with `--agents`.
pub const TOKEN_ENV: &str = "WSBLAST_AGENT_TOKEN";

/// One control message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Message {
    /// First message in each direction: the protocol version and the
    /// sender's wsblast version.
    Hello { protocol: u32, version: String },
    /// Coordinator to agent: this agent's share of the run. The agent waits
    /// up to `start_timeout` for `start` once it is ready.
    Prepare {
        config: Box<WireConfig>,
        start_timeout: Duration,
    },
    /// Agent to coordinator: the configuration is valid, the target is
    /// resolved and the agent waits for `start`.
    Ready { connections: usize },
    /// Coordinator to agent: start the run now.
    Start,
    /// Agent to coordinator, every second while the run lasts and once
    /// more just before `result`. `live` counts from the start of the test,
    /// across every phase of a scenario; `intervals` holds the latency
    /// histograms of the last two completed intervals of the phase.
    Snapshot {
        elapsed: Duration,
        live: WireSnapshot,
        #[serde(default)]
        intervals: Vec<WireInterval>,
    },
    /// Coordinator to agent: the new value of a refreshed authentication
    /// header, for every handshake from now on.
    Token { value: String },
    /// Agent to coordinator: capture records of the phase in progress.
    Capture { records: Vec<WireRecord> },
    /// Coordinator to agent: end the run early (Ctrl+C on the coordinator).
    /// Before `start` it cancels the test.
    Stop,
    /// Agent to coordinator: the outcome of the run. Always the last message
    /// an agent sends for a test.
    Result { result: Box<WireResult> },
    /// Either direction: the test cannot go on. The sender closes the
    /// connection after it.
    Error { message: String },
}

impl Message {
    /// The message as a text frame.
    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"type":"error","message":"message could not be encoded"}"#.to_string()
        })
    }

    /// Parses a text frame.
    pub(crate) fn decode(text: &str) -> std::result::Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("malformed control message: {e}"))
    }

    /// The `type` of the message, for error messages.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Prepare { .. } => "prepare",
            Self::Ready { .. } => "ready",
            Self::Start => "start",
            Self::Snapshot { .. } => "snapshot",
            Self::Token { .. } => "token",
            Self::Capture { .. } => "capture",
            Self::Stop => "stop",
            Self::Result { .. } => "result",
            Self::Error { .. } => "error",
        }
    }

    /// This build's hello.
    pub(crate) fn hello() -> Self {
        Self::Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Checks the peer's hello against this build's protocol version.
pub(crate) fn check_protocol(protocol: u32, version: &str) -> std::result::Result<(), String> {
    if protocol == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(format!(
        "protocol version mismatch: this is wsblast {} speaking protocol {PROTOCOL_VERSION}, the peer is wsblast {version} speaking protocol {protocol}; run the same wsblast version on the coordinator and every agent",
        env!("CARGO_PKG_VERSION")
    ))
}

/// Compares a presented token with the expected one in time that depends
/// only on the expected token's length, so the comparison does not reveal
/// how many leading bytes were right.
pub(crate) fn tokens_match(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = u8::from(expected.len() != presented.len());
    for (i, byte) in expected.iter().enumerate() {
        let other = presented.get(i).copied().unwrap_or(!*byte);
        diff |= byte ^ other;
    }
    std::hint::black_box(diff) == 0
}

/// Extracts the token of an `Authorization: Bearer <token>` header value.
pub(crate) fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// Rejects a token shorter than [`MIN_TOKEN_LEN`].
pub(crate) fn validate_token(token: &str) -> Result<()> {
    if token.chars().count() < MIN_TOKEN_LEN {
        return Err(ConfigError::InvalidOption(format!(
            "the agent token must be at least {MIN_TOKEN_LEN} characters long; generate one with e.g. `openssl rand -hex 32`"
        ))
        .into());
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ConfigError::InvalidOption(
            "the agent token must not contain whitespace or control characters".to_string(),
        )
        .into());
    }
    Ok(())
}

/// The part of a total that agent `index` of `agents` takes: the total
/// divided as evenly as possible, with the remainder going to the first
/// agents, one each.
pub(crate) fn split(total: u64, index: usize, agents: usize) -> u64 {
    let agents = agents.max(1) as u64;
    total / agents + u64::from((index as u64) < total % agents)
}

/// Which share of every stage target an agent opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Share {
    /// Position of the agent in `--agents`, from 0.
    pub(crate) index: usize,
    /// Number of agents.
    pub(crate) agents: usize,
}

impl Share {
    /// This agent's part of `total` connections. The parts of all agents add
    /// up to `total` for every value, so a staged run follows the requested
    /// profile at every moment, not only at the ends of the stages.
    pub(crate) fn of(self, total: usize) -> usize {
        split(total as u64, self.index, self.agents) as usize
    }
}

/// A stage on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireStage {
    pub(crate) duration: Duration,
    pub(crate) target: usize,
}

/// A payload on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WirePayload {
    /// A text payload, possibly a template with placeholders.
    Text { text: String },
    /// A binary payload, standard base64 with padding.
    Binary { base64: String },
}

/// One capture record as it travels from an agent to its coordinator: the
/// fields of a capture line, formatted on the agent, so the coordinator only
/// adds the agent and shifts the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireRecord {
    pub(crate) t_us: u64,
    pub(crate) worker: usize,
    pub(crate) dir: String,
    pub(crate) kind: String,
    pub(crate) len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<String>,
}

/// An action of a scenario phase on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WireAction {
    Send {
        payload: Option<WirePayload>,
        count: u64,
        #[serde(default)]
        expect: Option<crate::check::Check>,
        #[serde(default)]
        captures: Vec<crate::check::Capture>,
    },
    Wait {
        duration: Duration,
    },
    Ping {
        interval: Duration,
    },
    Close {
        code: u16,
        reason: String,
    },
}

/// What a scenario phase adds to a run: its name, think time and actions.
/// Its sizing, duration, rate and payload are already in the
/// [`WireConfig`] it travels in; its SLOs stay on the coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WirePhase {
    pub(crate) name: String,
    pub(crate) think_time: Duration,
    pub(crate) actions: Vec<WireAction>,
    /// The variables the phase captures, which its templates read.
    #[serde(default)]
    pub(crate) vars: Vec<String>,
}

/// Capture settings of an agent's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireCapture {
    /// Records this agent may still queue in this phase; `None` for no
    /// limit. The coordinator keeps the limit of the whole run exactly.
    pub(crate) limit: Option<u64>,
}

/// Chaos settings on the wire: the `--chaos-*` options of a run, or the
/// `chaos:` mapping of a scenario phase as the coordinator resolved it, with
/// the seed of this agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireChaos {
    pub(crate) delay: Duration,
    pub(crate) jitter: Duration,
    pub(crate) drop_percent: f64,
    pub(crate) corrupt_percent: f64,
    pub(crate) close_percent: f64,
    /// The `--chaos-close-mode` value, as spelled on the command line.
    pub(crate) close_mode: String,
    /// The `--chaos-read-pause` pause and jitter; zero from a coordinator
    /// that predates them.
    #[serde(default)]
    pub(crate) read_pause: Duration,
    #[serde(default)]
    pub(crate) read_jitter: Duration,
    /// This agent's seed, derived from the run's seed and the agent's
    /// position, so every agent draws decisions of its own.
    pub(crate) seed: u64,
}

#[cfg(feature = "chaos")]
impl WireChaos {
    /// The settings of `chaos`, with `seed` in place of its own.
    fn new(chaos: &crate::chaos::ChaosConfig, seed: u64) -> Self {
        Self {
            delay: chaos.delay,
            jitter: chaos.jitter,
            drop_percent: chaos.drop_percent,
            corrupt_percent: chaos.corrupt_percent,
            close_percent: chaos.close_percent,
            close_mode: chaos
                .close_mode
                .to_possible_value()
                .map(|value| value.get_name().to_string())
                .unwrap_or_default(),
            read_pause: chaos.read_pause,
            read_jitter: chaos.read_jitter,
            seed,
        }
    }

    /// The chaos configuration an agent runs with, checked as the command
    /// line checks it. `scripted` says whether the phase has an action
    /// script, which sends data frames in any mode.
    fn into_config(self, mode: TestMode, scripted: bool) -> Result<crate::chaos::ChaosConfig> {
        let close_mode = crate::chaos::ChaosCloseMode::from_str(&self.close_mode, false)
            .map_err(|_| unsupported(format!("unknown chaos close mode '{}'", self.close_mode)))?;
        let chaos = crate::chaos::ChaosConfig::new()
            .with_delay(self.delay, self.jitter)
            .with_drop(self.drop_percent)
            .with_corrupt(self.corrupt_percent)
            .with_close(self.close_percent, close_mode)
            .with_read_pause(self.read_pause, self.read_jitter)
            .with_seed(self.seed);
        chaos.validate()?;
        crate::config::validate_chaos_mode(&chaos, mode, scripted, "")?;
        Ok(chaos)
    }
}

/// A header on the wire: name and value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireHeader {
    pub(crate) name: String,
    pub(crate) value: String,
}

impl std::fmt::Debug for WireHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireHeader")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

/// GraphQL WS settings on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireGraphql {
    pub(crate) operation: String,
    pub(crate) operation_name: Option<String>,
    pub(crate) variables: Option<serde_json::Value>,
    pub(crate) extensions: Option<serde_json::Value>,
}

/// One agent's share of a run, as the coordinator sends it.
///
/// A dedicated type, so the wire format stays the same when
/// [`LoadTestConfig`] changes, and so only the settings an agent needs
/// travel: SLOs, report settings and file paths stay on the coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireConfig {
    pub(crate) target: String,
    /// The `--mode` value, as spelled on the command line.
    pub(crate) mode: String,
    /// Connections this agent opens; for a staged run, its share of the
    /// highest stage target.
    pub(crate) connections: usize,
    pub(crate) ramp_rate: u64,
    pub(crate) duration: Duration,
    /// The stages of the whole run; the agent opens `stage_share` of each
    /// target.
    pub(crate) stages: Vec<WireStage>,
    pub(crate) stage_share: Option<Share>,
    pub(crate) max_requests: Option<u64>,
    pub(crate) rate_per_conn: u64,
    pub(crate) payload: WirePayload,
    /// Handshake headers as name and value, including resolved credentials.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) subprotocol: Option<String>,
    pub(crate) connect_timeout: Duration,
    pub(crate) message_timeout: Duration,
    pub(crate) ping_interval: Duration,
    /// The `--match` value, as spelled on the command line.
    pub(crate) reply_match: String,
    /// The `--expect` check, if any.
    #[serde(default)]
    pub(crate) expect: Option<crate::check::Check>,
    pub(crate) reconnect: bool,
    pub(crate) graphql: Option<WireGraphql>,
    /// The scenario phase this share runs, if any.
    #[serde(default)]
    pub(crate) phase: Option<WirePhase>,
    /// Set when the coordinator captures frames.
    #[serde(default)]
    pub(crate) capture: Option<WireCapture>,
    /// The current value of an authentication header the coordinator
    /// refreshes; later values arrive in `token` messages.
    #[serde(default)]
    pub(crate) live_auth: Option<WireHeader>,
    /// The faults this agent injects, when the run or phase injects any.
    #[serde(default)]
    pub(crate) chaos: Option<WireChaos>,
    /// The run's data file, with this agent's place among the agents.
    #[serde(default)]
    pub(crate) data: Option<WireData>,
}

/// A data file on the wire: its rows, and which of them the agent's
/// connections use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireData {
    /// The file's name on the coordinator, for messages.
    pub(crate) path: String,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<String>>,
    /// Set by `plan`: agent `index` of `agents` uses every `agents`-th row
    /// from row `index`.
    pub(crate) share: Option<Share>,
}

fn unsupported(message: impl Into<String>) -> WsBlastError {
    ConfigError::UnsupportedOption(message.into()).into()
}

/// Splits `config` between `agents` agents, in `--agents` order.
///
/// Connections, `--requests` and `--ramp-rate` are divided with [`split`];
/// the message rate per connection is unchanged, so the total rate is the
/// same as on one machine. A staged run sends every agent the whole profile
/// and its [`Share`]. With chaos, every agent gets a seed of its own,
/// derived from the run's seed and its position.
///
/// # Errors
///
/// Returns a configuration error when a share would be zero (more agents
/// than connections, requests or ramp rate), or when a setting cannot be
/// sent.
pub(crate) fn plan(config: &LoadTestConfig, agents: usize) -> Result<Vec<WireConfig>> {
    if agents == 0 {
        return Err(ConfigError::InvalidOption("--agents lists no agent".to_string()).into());
    }
    if config.connections < agents {
        let what = if config.stages.is_empty() {
            format!("{} connections", config.connections)
        } else {
            format!("a highest stage target of {}", config.connections)
        };
        return Err(ConfigError::InvalidOption(format!(
            "{agents} agents cannot share {what}: every agent needs at least one connection; use fewer agents or more connections"
        ))
        .into());
    }
    if let Some(requests) = config.max_requests
        && requests < agents as u64
    {
        return Err(ConfigError::InvalidOption(format!(
            "{agents} agents cannot share -n {requests}: every agent needs at least one request"
        ))
        .into());
    }
    if config.ramp_rate > 0 && config.ramp_rate < agents as u64 {
        return Err(ConfigError::InvalidOption(format!(
            "{agents} agents cannot share --ramp-rate {}: every agent needs a ramp rate of at least 1 per second",
            config.ramp_rate
        ))
        .into());
    }

    let base = WireConfig::from_config(config)?;
    Ok((0..agents)
        .map(|index| {
            let share = Share { index, agents };
            let mut wire = base.clone();
            wire.connections = share.of(config.connections);
            wire.ramp_rate = split(config.ramp_rate, index, agents);
            wire.max_requests = config
                .max_requests
                .map(|requests| split(requests, index, agents));
            wire.stage_share = (!config.stages.is_empty()).then_some(share);
            if let Some(data) = wire.data.as_mut() {
                data.share = Some(share);
            }
            #[cfg(feature = "chaos")]
            if let Some(chaos) = &config.chaos {
                let seed = crate::chaos::agent_seed(chaos.seed.unwrap_or_default(), index);
                wire.chaos = Some(WireChaos::new(chaos, seed));
            }
            wire
        })
        .collect())
}

/// The command-line spelling of `mode`.
fn mode_name(mode: TestMode) -> String {
    mode.to_possible_value()
        .map(|value| value.get_name().to_string())
        .unwrap_or_default()
}

impl WireConfig {
    /// The settings of `config` that agents need, unsplit.
    pub(crate) fn from_config(config: &LoadTestConfig) -> Result<Self> {
        let payload = match &config.payload {
            PayloadConfig::StaticText(text) => WirePayload::Text {
                text: text.as_str().to_string(),
            },
            PayloadConfig::DynamicText(template) => WirePayload::Text {
                text: template.source().to_string(),
            },
            PayloadConfig::Binary(bytes) => WirePayload::Binary {
                base64: crate::base64::encode(bytes),
            },
        };
        let headers = config
            .headers
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
                    .map_err(|_| {
                        WsBlastError::from(ConfigError::InvalidHeader(format!(
                            "the value of header '{name}' is not visible ASCII and cannot be sent to agents"
                        )))
                    })
            })
            .chain(config.header_templates.iter().map(|(name, template)| {
                Ok((name.as_str().to_string(), template.source().to_string()))
            }))
            .collect::<Result<Vec<_>>>()?;
        let data = config.data.as_deref().map(|data| WireData {
            path: data.path().to_string(),
            columns: data.columns().to_vec(),
            rows: data.rows().to_vec(),
            share: None,
        });
        #[cfg(feature = "graphql-ws")]
        let graphql = config.graphql_ws.as_ref().map(|graphql| WireGraphql {
            operation: graphql.operation.clone(),
            operation_name: graphql.operation_name.clone(),
            variables: graphql.variables.clone(),
            extensions: graphql.extensions.clone(),
        });
        #[cfg(not(feature = "graphql-ws"))]
        let graphql = None;
        #[cfg(feature = "auth")]
        let live_auth = config
            .live_auth
            .as_ref()
            .map(|live| wire_header(live.name(), &live.current()))
            .transpose()?;
        #[cfg(not(feature = "auth"))]
        let live_auth = None;
        Ok(Self {
            target: config.target_url.to_string(),
            mode: mode_name(config.mode),
            connections: config.connections,
            ramp_rate: config.ramp_rate,
            duration: config.duration,
            stages: config
                .stages
                .iter()
                .map(|stage| WireStage {
                    duration: stage.duration,
                    target: stage.target,
                })
                .collect(),
            stage_share: None,
            max_requests: config.max_requests,
            rate_per_conn: config.rate_per_conn,
            payload,
            headers,
            subprotocol: config.subprotocol.clone(),
            connect_timeout: config.connect_timeout,
            message_timeout: config.message_timeout,
            ping_interval: config.ping_interval,
            reply_match: config.reply_match.flag_value(),
            expect: config.expect.clone(),
            reconnect: !config.no_reconnect,
            graphql,
            phase: None,
            capture: None,
            live_auth,
            // Set per agent by `plan`, with the agent's own seed.
            chaos: None,
            data,
        })
    }

    /// Builds the agent's configuration with the same validation as the
    /// command line.
    ///
    /// # Errors
    ///
    /// Returns the error [`LoadTestConfigBuilder::build`] returns, or a
    /// configuration error for a mode or setting this build does not
    /// support.
    ///
    /// [`LoadTestConfigBuilder::build`]: crate::LoadTestConfigBuilder::build
    pub(crate) fn into_config(self) -> Result<LoadTestConfig> {
        let mode = TestMode::from_str(&self.mode, false).map_err(|_| {
            unsupported(format!(
                "mode '{}' is not supported by this agent's build of wsblast",
                self.mode
            ))
        })?;
        let reply_match: ReplyMatch = self.reply_match.parse()?;
        let scripted = self
            .phase
            .as_ref()
            .is_some_and(|phase| !phase.actions.is_empty());
        let mut builder = LoadTestConfig::builder(self.target)
            .mode(mode)
            .connections(self.connections)
            .ramp_rate(self.ramp_rate)
            .duration(self.duration)
            .stages(
                self.stages
                    .iter()
                    .map(|stage| Stage::new(stage.duration, stage.target))
                    .collect(),
            )
            .rate_per_connection(self.rate_per_conn)
            .connect_timeout(self.connect_timeout)
            .message_timeout(self.message_timeout)
            .ping_interval(self.ping_interval)
            .reply_match(reply_match)
            .reconnect(self.reconnect)
            .progress(false);
        if let Some(requests) = self.max_requests {
            builder = builder.max_requests(requests);
        }
        if let Some(check) = &self.expect {
            builder = builder.expect(check.flag_value());
        }
        builder = match self.payload {
            WirePayload::Text { text } => builder.payload_text(text),
            WirePayload::Binary { base64 } => {
                let bytes = crate::base64::decode(&base64).ok_or_else(|| {
                    ConfigError::InvalidOption("the binary payload is not valid base64".to_string())
                })?;
                builder.payload_binary(bytes)
            }
        };
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(data) = self.data {
            let mut set = crate::data::DataSet::from_parts(data.path, data.columns, data.rows)
                .map_err(ConfigError::DataFile)?;
            if let Some(share) = data.share {
                if share.agents == 0 || share.index >= share.agents {
                    return Err(ConfigError::InvalidOption(format!(
                        "invalid data share {} of {}",
                        share.index, share.agents
                    ))
                    .into());
                }
                set = set.with_share(share.index, share.agents);
            }
            builder = builder.data_set(set);
        }
        if let Some(subprotocol) = self.subprotocol {
            builder = builder.subprotocol(subprotocol);
        }
        if let Some(graphql) = self.graphql {
            #[cfg(feature = "graphql-ws")]
            {
                builder = builder.graphql_operation(graphql.operation);
                if let Some(name) = graphql.operation_name {
                    builder = builder.graphql_operation_name(name);
                }
                if let Some(variables) = graphql.variables {
                    builder = builder.graphql_variables(variables);
                }
                if let Some(extensions) = graphql.extensions {
                    builder = builder.graphql_extensions(extensions);
                }
            }
            #[cfg(not(feature = "graphql-ws"))]
            {
                let _ = graphql;
                return Err(unsupported(
                    "GraphQL WS settings need the 'graphql-ws' feature, which this agent's build of wsblast does not include",
                ));
            }
        }
        let mut config = builder.build()?;
        if let Some(header) = self.live_auth {
            #[cfg(feature = "auth")]
            {
                let (name, value) = header.parse()?;
                config.live_auth = Some(crate::auth::LiveAuthHeader::received(name, value));
            }
            #[cfg(not(feature = "auth"))]
            {
                let _ = header;
                return Err(unsupported(
                    "a refreshed authentication token needs the 'auth' feature, which this agent's build of wsblast does not include",
                ));
            }
        }
        if let Some(chaos) = self.chaos {
            #[cfg(feature = "chaos")]
            {
                config.chaos = Some(chaos.into_config(mode, scripted)?);
            }
            #[cfg(not(feature = "chaos"))]
            {
                let _ = (chaos, scripted);
                return Err(unsupported(
                    "chaos injection needs the 'chaos' feature, which this agent's build of wsblast does not include",
                ));
            }
        }
        #[cfg(not(feature = "chaos"))]
        let _ = scripted;
        if let Some(share) = self.stage_share {
            if share.agents == 0 || share.index >= share.agents {
                return Err(ConfigError::InvalidOption(format!(
                    "invalid stage share {} of {}",
                    share.index, share.agents
                ))
                .into());
            }
            if config.stages.is_empty() {
                return Err(
                    ConfigError::InvalidOption("a stage share needs stages".to_string()).into(),
                );
            }
            config.connections = share.of(config.connections);
            config.stage_share = Some(share);
        }
        if config.connections == 0 {
            return Err(ConfigError::ZeroConcurrency(0).into());
        }
        Ok(config)
    }
}

/// A header for the wire. The value is never part of an error message.
#[cfg(feature = "auth")]
pub(crate) fn wire_header(
    name: &http::HeaderName,
    value: &http::HeaderValue,
) -> Result<WireHeader> {
    let value = value.to_str().map_err(|_| {
        WsBlastError::from(ConfigError::InvalidHeader(format!(
            "the value of header '{name}' is not visible ASCII and cannot be sent to agents"
        )))
    })?;
    Ok(WireHeader {
        name: name.as_str().to_string(),
        value: value.to_string(),
    })
}

impl WireHeader {
    /// The header name and a sensitive value. The value is never part of an
    /// error message.
    #[cfg(feature = "auth")]
    pub(crate) fn parse(&self) -> Result<(http::HeaderName, http::HeaderValue)> {
        let name = http::HeaderName::from_bytes(self.name.as_bytes()).map_err(|_| {
            ConfigError::InvalidHeader(format!("invalid header name '{}'", self.name))
        })?;
        Ok((name, sensitive_value(&self.value)?))
    }
}

/// A header value marked sensitive. The value is never part of an error
/// message.
#[cfg(feature = "auth")]
pub(crate) fn sensitive_value(value: &str) -> Result<http::HeaderValue> {
    let mut value = http::HeaderValue::from_str(value).map_err(|_| {
        ConfigError::InvalidHeader(
            "the refreshed authentication header value is not a valid header value".to_string(),
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}

#[cfg(feature = "scenario")]
fn wire_payload(payload: &PayloadConfig) -> WirePayload {
    match payload {
        PayloadConfig::StaticText(text) => WirePayload::Text {
            text: text.as_str().to_string(),
        },
        PayloadConfig::DynamicText(template) => WirePayload::Text {
            text: template.source().to_string(),
        },
        PayloadConfig::Binary(bytes) => WirePayload::Binary {
            base64: crate::base64::encode(bytes),
        },
    }
}

#[cfg(feature = "scenario")]
fn payload_from_wire(payload: WirePayload, vars: &[String]) -> Result<PayloadConfig> {
    match payload {
        WirePayload::Text { text } => Ok(PayloadConfig::from_text_with_vars(text, vars)),
        WirePayload::Binary { base64 } => crate::base64::decode(&base64)
            .map(|bytes| PayloadConfig::Binary(bytes.into()))
            .ok_or_else(|| {
                ConfigError::InvalidOption("a binary payload is not valid base64".to_string())
                    .into()
            }),
    }
}

#[cfg(feature = "scenario")]
impl WirePhase {
    /// The parts of `phase` an agent needs besides its share.
    pub(crate) fn from_phase(phase: &crate::config::ScenarioPhase) -> Self {
        use crate::config::ScenarioAction;
        Self {
            name: phase.name.clone(),
            think_time: phase.think_time,
            actions: phase
                .actions
                .iter()
                .map(|action| match action {
                    ScenarioAction::Send {
                        payload,
                        count,
                        expect,
                        captures,
                    } => WireAction::Send {
                        payload: payload.as_ref().map(wire_payload),
                        count: *count,
                        expect: expect.clone(),
                        captures: captures.clone(),
                    },
                    ScenarioAction::Wait(duration) => WireAction::Wait {
                        duration: *duration,
                    },
                    ScenarioAction::Ping(interval) => WireAction::Ping {
                        interval: *interval,
                    },
                    ScenarioAction::Close { code, reason } => WireAction::Close {
                        code: *code,
                        reason: reason.clone(),
                    },
                })
                .collect(),
            vars: phase.vars.clone(),
        }
    }

    /// Rebuilds the phase an agent runs with `config`, its share: sizing,
    /// duration, ramp, stages, rate and payload come from `config`, so the
    /// runner keeps the agent's share. SLOs are checked on the coordinator.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an action the command line would
    /// reject.
    pub(crate) fn into_phase(
        self,
        config: &LoadTestConfig,
    ) -> Result<crate::config::ScenarioPhase> {
        use crate::config::ScenarioAction;
        let invalid = |reason: &str| -> WsBlastError {
            ConfigError::InvalidScenario(format!("phase '{}': {reason}", self.name)).into()
        };
        let vars = self.vars.clone();
        for name in &vars {
            crate::check::check_var_name(name).map_err(|e| invalid(&format!("capture: {e}")))?;
        }
        let actions = self
            .actions
            .into_iter()
            .map(|action| {
                Ok(match action {
                    WireAction::Send {
                        payload,
                        count,
                        expect,
                        captures,
                    } => {
                        if count == 0 {
                            return Err(invalid("send action count must be greater than zero"));
                        }
                        if let Some(check) = &expect {
                            check
                                .validate()
                                .map_err(|e| invalid(&format!("expect: {e}")))?;
                        }
                        if let Some(capture) = captures.iter().find(|c| c.var >= vars.len()) {
                            return Err(invalid(&format!(
                                "capture into variable {} of {}",
                                capture.var,
                                vars.len()
                            )));
                        }
                        ScenarioAction::Send {
                            payload: payload
                                .map(|payload| payload_from_wire(payload, &vars))
                                .transpose()?,
                            count,
                            expect,
                            captures,
                        }
                    }
                    WireAction::Wait { duration } if duration.is_zero() => {
                        return Err(invalid("wait action duration must be greater than zero"));
                    }
                    WireAction::Wait { duration } => ScenarioAction::Wait(duration),
                    WireAction::Ping { interval } if interval.is_zero() => {
                        return Err(invalid("ping action interval must be greater than zero"));
                    }
                    WireAction::Ping { interval } => ScenarioAction::Ping(interval),
                    WireAction::Close { code, reason } => ScenarioAction::Close {
                        code: crate::config::validate_close_code(code)?,
                        reason,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(crate::config::ScenarioPhase {
            name: self.name,
            target: None,
            duration: config.duration,
            connections: config.connections,
            ramp_rate: config.ramp_rate,
            rate_per_conn: config.rate_per_conn,
            // The share's payload was parsed without the phase's variables;
            // parsed again with them, `{{NAME}}` reads the captures.
            payload: Some(match &config.payload {
                PayloadConfig::StaticText(text) if !vars.is_empty() => {
                    PayloadConfig::from_text_with_vars(text.as_str().to_string(), &vars)
                }
                PayloadConfig::DynamicText(template) if !vars.is_empty() => {
                    PayloadConfig::from_text_with_vars(template.source().to_string(), &vars)
                }
                payload => payload.clone(),
            }),
            think_time: self.think_time,
            actions,
            vars,
            slo: crate::config::SloThresholds::default(),
            stages: config.stages.clone(),
            // The coordinator resolved the phase's chaos (its own mapping
            // or the command line's, and the seed) into the share's
            // configuration, with this agent's seed. The phase carries
            // exactly that, so the runner neither falls back to other
            // settings nor picks another seed; no chaos at all is an empty
            // mapping, which turns chaos off.
            #[cfg(feature = "chaos")]
            chaos: Some(config.chaos.clone().unwrap_or_default()),
        })
    }
}

/// The live counters on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireSnapshot {
    pub(crate) active_connections: u64,
    pub(crate) connections_established: u64,
    pub(crate) connections_failed: u64,
    pub(crate) reconnects: u64,
    pub(crate) messages_sent: u64,
    pub(crate) messages_received: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) bytes_received: u64,
    pub(crate) total_errors: u64,
    pub(crate) unsolicited_frames: u64,
}

impl From<LiveSnapshot> for WireSnapshot {
    fn from(snap: LiveSnapshot) -> Self {
        Self {
            active_connections: snap.active_connections,
            connections_established: snap.connections_established,
            connections_failed: snap.connections_failed,
            reconnects: snap.reconnects,
            messages_sent: snap.messages_sent,
            messages_received: snap.messages_received,
            bytes_sent: snap.bytes_sent,
            bytes_received: snap.bytes_received,
            total_errors: snap.total_errors,
            unsolicited_frames: snap.unsolicited_frames,
        }
    }
}

impl WireSnapshot {
    /// Adds another agent's counters.
    pub(crate) fn add(&mut self, other: &Self) {
        self.active_connections += other.active_connections;
        self.connections_established += other.connections_established;
        self.connections_failed += other.connections_failed;
        self.reconnects += other.reconnects;
        self.messages_sent += other.messages_sent;
        self.messages_received += other.messages_received;
        self.bytes_sent += other.bytes_sent;
        self.bytes_received += other.bytes_received;
        self.total_errors += other.total_errors;
        self.unsolicited_frames += other.unsolicited_frames;
    }
}

/// A histogram as its recorded values: `(value, count)` for every bucket
/// with a count, where `value` is the highest value the bucket holds.
/// Recording these into an empty histogram with the same bounds and
/// precision gives the same buckets, so every percentile, the minimum, the
/// maximum and the mean come out exactly as on the agent.
pub(crate) type Sparse = Vec<(u64, u64)>;

/// The recorded values of `hist`.
pub(crate) fn to_sparse(hist: &Histogram<u64>) -> Sparse {
    hist.iter_recorded()
        .map(|value| {
            (
                value.value_iterated_to(),
                value.count_since_last_iteration(),
            )
        })
        .collect()
}

/// Records `sparse` into `empty`.
pub(crate) fn from_sparse(
    sparse: &[(u64, u64)],
    mut empty: Histogram<u64>,
) -> std::result::Result<Histogram<u64>, String> {
    for &(value, count) in sparse {
        empty
            .record_n(value, count)
            .map_err(|e| format!("histogram value {value} out of range: {e:?}"))?;
    }
    Ok(empty)
}

/// An agent's [`MetricsTotals`] on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireTotals {
    pub(crate) messages_sent: u64,
    pub(crate) messages_recv: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) bytes_recv: u64,
    pub(crate) reconnects: u64,
    pub(crate) unsolicited_frames: u64,
    pub(crate) sent_message_errors: u64,
    pub(crate) interrupted: u64,
    pub(crate) errors: HashMap<ErrorCategory, u64>,
    pub(crate) handshake: Sparse,
    pub(crate) rtt: Sparse,
    pub(crate) response: Sparse,
    /// The faults the agent injected, with its own seed.
    #[serde(default)]
    pub(crate) chaos: Option<ChaosSummary>,
}

impl From<&MetricsTotals> for WireTotals {
    fn from(totals: &MetricsTotals) -> Self {
        Self {
            messages_sent: totals.messages_sent,
            messages_recv: totals.messages_recv,
            bytes_sent: totals.bytes_sent,
            bytes_recv: totals.bytes_recv,
            reconnects: totals.reconnects,
            unsolicited_frames: totals.unsolicited_frames,
            sent_message_errors: totals.sent_message_errors,
            interrupted: totals.interrupted,
            errors: totals.errors.clone(),
            handshake: to_sparse(&totals.handshake),
            rtt: to_sparse(&totals.rtt),
            response: to_sparse(&totals.response),
            chaos: totals.chaos.clone(),
        }
    }
}

impl WireTotals {
    /// Rebuilds the totals, histograms included.
    pub(crate) fn into_totals(self) -> std::result::Result<MetricsTotals, String> {
        let empty = || MetricsTotals::empty_histogram().map_err(|e| e.to_string());
        Ok(MetricsTotals {
            handshake: from_sparse(&self.handshake, empty()?)?,
            rtt: from_sparse(&self.rtt, empty()?)?,
            response: from_sparse(&self.response, empty()?)?,
            messages_sent: self.messages_sent,
            messages_recv: self.messages_recv,
            bytes_sent: self.bytes_sent,
            bytes_recv: self.bytes_recv,
            reconnects: self.reconnects,
            unsolicited_frames: self.unsolicited_frames,
            sent_message_errors: self.sent_message_errors,
            interrupted: self.interrupted,
            errors: self.errors,
            chaos: self.chaos,
            absorbed: Vec::new(),
        })
    }
}

/// The latency histogram of one interval of an agent's time series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireInterval {
    /// Interval index, counted from the agent's start.
    pub(crate) index: u64,
    pub(crate) values: Sparse,
}

/// What an agent's generator lag probe measured during its run: every
/// wake-up's lag as the recorded values of its histogram, the time-series
/// intervals with a sample, those over the threshold, and the threshold of
/// the agent's platform, which they were checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireLag {
    pub(crate) values: Sparse,
    pub(crate) intervals: u64,
    pub(crate) intervals_over: u64,
    pub(crate) threshold_us: u64,
}

impl WireLag {
    fn new(lag: &LagTotals) -> Option<Self> {
        let (hist, intervals, intervals_over, threshold_us) = lag.parts();
        Some(Self {
            values: to_sparse(hist?),
            intervals,
            intervals_over,
            threshold_us,
        })
    }

    fn decode(self) -> std::result::Result<LagTotals, String> {
        let empty = crate::lag::lag_histogram().ok_or("cannot create a lag histogram")?;
        Ok(LagTotals::from_parts(
            from_sparse(&self.values, empty)?,
            self.intervals,
            self.intervals_over,
            self.threshold_us,
        ))
    }
}

/// The result of an agent's run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireResult {
    /// The agent's own run time, from its start to the end of its run.
    pub(crate) elapsed: Duration,
    /// The agent's wall clock when it started, in milliseconds since the
    /// Unix epoch. Informational: agents' clocks are not synchronised.
    pub(crate) started_at_unix_ms: u64,
    /// Connection slots the agent was configured to open.
    pub(crate) connections: u64,
    /// Slots its ramp had not reached when the run ended.
    pub(crate) not_started: u64,
    pub(crate) totals: WireTotals,
    pub(crate) time_series: TimeSeries,
    pub(crate) intervals: Vec<WireInterval>,
    /// Capture records the agent dropped: at its limit, on a full queue or
    /// because the control channel was gone. `None` without capture.
    #[serde(default)]
    pub(crate) capture_dropped: Option<u64>,
    /// The agent's generator lag; `None` when its probe took no sample.
    #[serde(default)]
    pub(crate) generator_lag: Option<WireLag>,
    /// The agent's open file limit against what its share needed; `None`
    /// where the agent cannot read it (Windows).
    #[serde(default)]
    pub(crate) open_file_limit: Option<OpenFileLimit>,
}

/// A decoded result, ready to merge.
pub(crate) struct AgentResult {
    pub(crate) elapsed: Duration,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) connections: u64,
    pub(crate) not_started: u64,
    pub(crate) totals: MetricsTotals,
    pub(crate) series: SeriesPart,
    pub(crate) capture_dropped: Option<u64>,
    pub(crate) lag: LagTotals,
    pub(crate) open_file_limit: Option<OpenFileLimit>,
}

impl WireResult {
    /// Packs an agent's merged totals and its time series.
    pub(crate) fn new(
        totals: &MetricsTotals,
        outcome_meta: ResultMeta,
        time_series: TimeSeries,
        buckets: &BTreeMap<u64, Histogram<u64>>,
    ) -> Self {
        Self {
            elapsed: outcome_meta.elapsed,
            started_at_unix_ms: outcome_meta.started_at_unix_ms,
            connections: outcome_meta.connections,
            not_started: outcome_meta.not_started,
            totals: WireTotals::from(totals),
            time_series,
            intervals: buckets
                .iter()
                .map(|(index, hist)| WireInterval {
                    index: *index,
                    values: to_sparse(hist),
                })
                .collect(),
            capture_dropped: outcome_meta.capture_dropped,
            generator_lag: WireLag::new(&outcome_meta.lag),
            open_file_limit: outcome_meta.open_file_limit,
        }
    }

    /// Rebuilds the histograms.
    pub(crate) fn decode(self) -> std::result::Result<AgentResult, String> {
        let mut buckets = BTreeMap::new();
        for interval in self.intervals {
            let empty = interval_histogram().ok_or("cannot create an interval histogram")?;
            buckets.insert(interval.index, from_sparse(&interval.values, empty)?);
        }
        Ok(AgentResult {
            elapsed: self.elapsed,
            started_at_unix_ms: self.started_at_unix_ms,
            connections: self.connections,
            not_started: self.not_started,
            totals: self.totals.into_totals()?,
            series: SeriesPart {
                series: self.time_series,
                buckets,
            },
            capture_dropped: self.capture_dropped,
            lag: self
                .generator_lag
                .map(WireLag::decode)
                .transpose()?
                .unwrap_or_default(),
            open_file_limit: self.open_file_limit,
        })
    }
}

/// The counts of a result that do not come from the workers.
pub(crate) struct ResultMeta {
    pub(crate) elapsed: Duration,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) connections: u64,
    pub(crate) not_started: u64,
    pub(crate) capture_dropped: Option<u64>,
    pub(crate) lag: LagTotals,
    pub(crate) open_file_limit: Option<OpenFileLimit>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AggregatedMetrics, LiveMetrics, WorkerMetrics};

    fn config(builder: crate::LoadTestConfigBuilder) -> LoadTestConfig {
        builder.build().expect("valid config")
    }

    #[test]
    fn split_is_even_with_the_remainder_on_the_first_agents() {
        let parts: Vec<u64> = (0..3).map(|i| split(10, i, 3)).collect();
        assert_eq!(parts, [4, 3, 3]);
        let parts: Vec<u64> = (0..4).map(|i| split(12, i, 4)).collect();
        assert_eq!(parts, [3, 3, 3, 3]);
        let parts: Vec<u64> = (0..3).map(|i| split(2, i, 3)).collect();
        assert_eq!(parts, [1, 1, 0]);
        for total in 0..200u64 {
            for agents in 1..9 {
                let sum: u64 = (0..agents).map(|i| split(total, i, agents)).sum();
                assert_eq!(sum, total, "{total} over {agents}");
            }
        }
    }

    #[test]
    fn plan_divides_connections_requests_and_ramp() {
        let base = LoadTestConfig::builder("ws://127.0.0.1:9/")
            .connections(10)
            .ramp_rate(7)
            .max_requests(101)
            .rate_per_connection(20);
        let shares = plan(&config(base), 3).unwrap();
        let conns: Vec<usize> = shares.iter().map(|s| s.connections).collect();
        assert_eq!(conns, [4, 3, 3]);
        let ramps: Vec<u64> = shares.iter().map(|s| s.ramp_rate).collect();
        assert_eq!(ramps, [3, 2, 2]);
        let requests: Vec<Option<u64>> = shares.iter().map(|s| s.max_requests).collect();
        assert_eq!(requests, [Some(34), Some(34), Some(33)]);
        assert!(shares.iter().all(|s| s.rate_per_conn == 20));
        assert!(shares.iter().all(|s| s.stage_share.is_none()));
    }

    #[test]
    fn plan_rejects_zero_shares() {
        let few = config(LoadTestConfig::builder("ws://127.0.0.1:9/").connections(2));
        let err = plan(&few, 3).unwrap_err().to_string();
        assert!(err.contains("3 agents cannot share 2 connections"), "{err}");

        let requests = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .connections(9)
                .max_requests(2),
        );
        assert!(plan(&requests, 3).unwrap_err().to_string().contains("-n 2"));

        let ramp = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .connections(9)
                .ramp_rate(2),
        );
        assert!(
            plan(&ramp, 3)
                .unwrap_err()
                .to_string()
                .contains("--ramp-rate 2")
        );

        let stages = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .stages(vec![Stage::new(Duration::from_secs(1), 2)]),
        );
        assert!(
            plan(&stages, 3)
                .unwrap_err()
                .to_string()
                .contains("highest stage target of 2")
        );
    }

    #[test]
    fn staged_shares_add_up_to_the_target_at_every_moment() {
        let stages = vec![
            Stage::new(Duration::from_secs(10), 17),
            Stage::new(Duration::from_secs(5), 17),
            Stage::new(Duration::from_secs(7), 3),
        ];
        let whole = config(LoadTestConfig::builder("ws://127.0.0.1:9/").stages(stages.clone()));
        let agents: Vec<LoadTestConfig> = plan(&whole, 3)
            .unwrap()
            .into_iter()
            .map(|wire| wire.into_config().unwrap())
            .collect();
        let peaks: Vec<usize> = agents.iter().map(|c| c.connections).collect();
        assert_eq!(peaks, [6, 6, 5]);
        for ms in (0..=23_000).step_by(37) {
            let at = Duration::from_millis(ms);
            let sum: usize = agents.iter().map(|c| c.stage_target(at)).sum();
            assert_eq!(sum, Stage::target_at(&stages, at), "at {at:?}");
        }
    }

    #[test]
    fn agents_get_the_data_rows_and_use_different_ones() {
        let data = crate::data::DataSet::parse("users.csv", "user\na\nb\nc\nd\ne\nf\n").unwrap();
        let whole = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .connections(6)
                .data_set(data)
                .payload_text("{{data.user}}")
                .header("X-User", "{{data.user}}"),
        );
        let wires = plan(&whole, 2).unwrap();
        // Templated headers travel as their template.
        assert!(
            wires[0]
                .headers
                .contains(&("x-user".to_string(), "{{data.user}}".to_string()))
        );
        let users: Vec<Vec<String>> = wires
            .iter()
            .map(|wire| {
                let agent = wire.clone().into_config().unwrap();
                assert_eq!(agent.header_templates.len(), 1);
                let data = agent.data.clone().unwrap();
                (0..agent.connections)
                    .map(|worker| {
                        data.value(data.row_for(worker), "user")
                            .unwrap()
                            .to_string()
                    })
                    .collect()
            })
            .collect();
        assert_eq!(users, [["a", "c", "e"], ["b", "d", "f"]]);
    }

    #[test]
    fn wire_config_round_trips_through_json_and_validation() {
        let original = config(
            LoadTestConfig::builder("wss://example.com/socket?x=1")
                .connections(8)
                .duration(Duration::from_millis(2_500))
                .rate_per_connection(5)
                .payload_text(r#"{"seq":{{seq}}}"#)
                .reply_match("json:/seq".parse().unwrap())
                .header("Authorization", "Bearer secret")
                .header("X-Multi", "a")
                .header("X-Multi", "b")
                .subprotocol("chat")
                .connect_timeout(Duration::from_secs(2))
                .message_timeout(Duration::from_millis(750))
                .ping_interval(Duration::from_secs(3))
                .reconnect(false),
        );
        let wire = WireConfig::from_config(&original).unwrap();
        let message = Message::Prepare {
            config: Box::new(wire.clone()),
            start_timeout: Duration::from_secs(30),
        };
        let Message::Prepare {
            config: decoded, ..
        } = Message::decode(&message.encode()).unwrap()
        else {
            panic!("not a prepare message");
        };
        assert_eq!(*decoded, wire);
        let rebuilt = decoded.into_config().unwrap();
        assert_eq!(rebuilt.target_url, original.target_url);
        assert_eq!(rebuilt.connections, 8);
        assert_eq!(rebuilt.duration, original.duration);
        assert_eq!(rebuilt.rate_per_conn, 5);
        assert_eq!(rebuilt.reply_match, original.reply_match);
        assert_eq!(rebuilt.headers, original.headers);
        assert_eq!(rebuilt.subprotocol.as_deref(), Some("chat"));
        assert_eq!(rebuilt.connect_timeout, original.connect_timeout);
        assert_eq!(rebuilt.message_timeout, original.message_timeout);
        assert_eq!(rebuilt.ping_interval, original.ping_interval);
        assert!(rebuilt.no_reconnect);
        assert!(
            matches!(rebuilt.payload, PayloadConfig::DynamicText(ref t) if t.source() == r#"{"seq":{{seq}}}"#)
        );
    }

    #[test]
    fn binary_payloads_travel_as_base64() {
        let bytes = vec![0u8, 1, 2, 254, 255, 10, 13];
        let original = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .mode(TestMode::Stream)
                .payload_binary(bytes.clone()),
        );
        let wire = WireConfig::from_config(&original).unwrap();
        assert!(matches!(wire.payload, WirePayload::Binary { .. }));
        let rebuilt = wire.into_config().unwrap();
        assert_eq!(rebuilt.mode, TestMode::Stream);
        assert!(matches!(rebuilt.payload, PayloadConfig::Binary(ref b) if b[..] == bytes[..]));
    }

    #[test]
    fn agents_validate_the_config_they_receive() {
        let mut wire =
            WireConfig::from_config(&config(LoadTestConfig::builder("ws://127.0.0.1:9/"))).unwrap();
        wire.target = "http://127.0.0.1:9/".to_string();
        assert!(matches!(
            wire.clone().into_config(),
            Err(WsBlastError::Config(ConfigError::UnsupportedScheme(_)))
        ));
        wire.target = "ws://127.0.0.1:9/".to_string();
        wire.mode = "carrier-pigeon".to_string();
        assert!(wire.clone().into_config().is_err());
        wire.mode = "listen".to_string();
        wire.reply_match = "exact".to_string();
        assert!(wire.clone().into_config().is_err());
        wire.reply_match = "first".to_string();
        wire.connections = 0;
        assert!(wire.into_config().is_err());
    }

    fn worker(live: &std::sync::Arc<LiveMetrics>, seed: u64) -> WorkerMetrics {
        let mut w = WorkerMetrics::new(std::sync::Arc::clone(live)).unwrap();
        w.record_handshake(Duration::from_micros(800 + seed * 13));
        for i in 0..500u64 {
            w.record_message_sent(32);
            let micros = (i * 7919 + seed * 104_729) % 90_000 + 1;
            w.record_message_recv(32, Some(Duration::from_micros(micros)));
            w.record_response_time(Duration::from_micros(micros + 17));
        }
        w.record_error(
            ErrorCategory::Timeout,
            crate::metrics::ErrorScope::SentMessage,
        );
        w.record_connection_failed(ErrorCategory::TcpConnect);
        w
    }

    #[test]
    fn sparse_histograms_preserve_every_statistic_exactly() {
        let live = LiveMetrics::new();
        let workers: Vec<WorkerMetrics> = (0..6).map(|seed| worker(&live, seed)).collect();
        let single = AggregatedMetrics::merge(workers.clone(), Duration::from_secs(2), 6).unwrap();

        // Three agents with two workers each, sent over the wire as JSON.
        let mut merged = MetricsTotals::new().unwrap();
        for pair in workers.chunks(2) {
            let mut totals = MetricsTotals::new().unwrap();
            for w in pair {
                totals.add_worker(w.clone());
            }
            let json = serde_json::to_string(&WireTotals::from(&totals)).unwrap();
            let decoded: WireTotals = serde_json::from_str(&json).unwrap();
            merged.add(&decoded.into_totals().unwrap()).unwrap();
        }
        let distributed = merged.finish(Duration::from_secs(2), 6);
        assert_eq!(distributed, single);
        assert!(single.message_rtt.p999_us > single.message_rtt.p50_us);
    }

    #[test]
    fn interval_histograms_round_trip() {
        let mut hist = interval_histogram().unwrap();
        for v in [1, 5, 999, 1_234, 56_789, 59_000_000] {
            hist.record_n(v, v % 7 + 1).unwrap();
        }
        let back = from_sparse(&to_sparse(&hist), interval_histogram().unwrap()).unwrap();
        assert_eq!(back, hist);
        for q in [0.5, 0.9, 0.99, 1.0] {
            assert_eq!(back.value_at_quantile(q), hist.value_at_quantile(q));
        }
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        let empty = MetricsTotals::empty_histogram().unwrap();
        assert!(from_sparse(&[(u64::MAX, 1)], empty).is_err());
    }

    #[test]
    fn token_comparison() {
        assert!(tokens_match("0123456789abcdef", "0123456789abcdef"));
        assert!(!tokens_match("0123456789abcdef", "0123456789abcdeg"));
        assert!(!tokens_match("0123456789abcdef", "0123456789abcde"));
        assert!(!tokens_match("0123456789abcdef", "0123456789abcdef0"));
        assert!(!tokens_match("0123456789abcdef", ""));
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert!(validate_token("short").is_err());
        assert!(validate_token("0123456789abcde f").is_err());
        assert!(validate_token("0123456789abcdef").is_ok());
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn scenario_phases_travel_with_their_actions_and_the_agents_share() {
        let scenario = crate::ScenarioConfig::from_yaml(
            r#"
version: "1.0"
target: "ws://127.0.0.1:9/"
metadata:
  name: "wire"
phases:
  - name: "scripted"
    connections: 9
    duration: "2s"
    rate_per_conn: 4
    think_time: "15ms"
    payload: "base"
    actions:
      - type: Send
        payload: "{{seq}}"
        count: 3
      - type: Wait
        duration: "50ms"
      - type: Ping
        interval: "1s"
      - type: Close
        code: 4001
        reason: "bye"
"#,
        )
        .unwrap();
        let phase = scenario.compile(None).unwrap().remove(0);
        let base = config(LoadTestConfig::builder("ws://127.0.0.1:9/"));
        let whole = crate::runner::phase_config(base, &phase);
        let mut shares = plan(&whole, 2).unwrap();
        let mut share = shares.remove(1);
        share.phase = Some(WirePhase::from_phase(&phase));
        let message = Message::Prepare {
            config: Box::new(share),
            start_timeout: Duration::from_secs(1),
        };
        let Message::Prepare { config: wire, .. } = Message::decode(&message.encode()).unwrap()
        else {
            panic!("not a prepare message");
        };
        let mut wire = *wire;
        let wire_phase = wire.phase.take().unwrap();
        let agent = wire.into_config().unwrap();
        let rebuilt = wire_phase.into_phase(&agent).unwrap();
        assert_eq!(rebuilt.name, "scripted");
        assert_eq!(rebuilt.connections, 4, "the agent's share of 9");
        assert_eq!(rebuilt.duration, Duration::from_secs(2));
        assert_eq!(rebuilt.rate_per_conn, 4);
        assert_eq!(rebuilt.think_time, Duration::from_millis(15));
        assert_eq!(rebuilt.actions.len(), 4);
        assert!(matches!(
            &rebuilt.actions[0],
            crate::config::ScenarioAction::Send { payload: Some(PayloadConfig::DynamicText(t)), count: 3, .. } if t.source() == "{{seq}}"
        ));
        assert!(matches!(
            rebuilt.actions[1],
            crate::config::ScenarioAction::Wait(d) if d == Duration::from_millis(50)
        ));
        assert!(matches!(
            &rebuilt.actions[3],
            crate::config::ScenarioAction::Close { code: 4001, reason } if reason == "bye"
        ));
        assert!(
            matches!(rebuilt.payload, Some(PayloadConfig::StaticText(ref t)) if t.as_str() == "base")
        );
        assert_eq!(rebuilt.slo, crate::config::SloThresholds::default());

        let mut bad = WirePhase::from_phase(&phase);
        bad.actions = vec![WireAction::Close {
            code: 1005,
            reason: String::new(),
        }];
        assert!(bad.into_phase(&agent).is_err());
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn captures_checks_and_variables_reach_the_agent() {
        let scenario = crate::ScenarioConfig::from_yaml(
            r#"
version: "1.0"
target: "ws://127.0.0.1:9/"
metadata:
  name: "wire"
phases:
  - name: "session"
    connections: 2
    duration: "1s"
    payload: '{"op":"sub","s":"{{session}}"}'
    actions:
      - type: Send
        payload: '{"op":"login"}'
        expect: { json: /status, equals: ok }
        capture: { session: /session }
      - type: Send
"#,
        )
        .unwrap();
        let phase = scenario.compile(None).unwrap().remove(0);
        let base = config(LoadTestConfig::builder("ws://127.0.0.1:9/").expect("contains:ok"));
        let whole = crate::runner::phase_config(base, &phase);
        let mut share = plan(&whole, 2).unwrap().remove(0);
        share.phase = Some(WirePhase::from_phase(&phase));
        let Message::Prepare { config: wire, .. } = Message::decode(
            &Message::Prepare {
                config: Box::new(share),
                start_timeout: Duration::from_secs(30),
            }
            .encode(),
        )
        .unwrap() else {
            panic!("not a prepare message");
        };
        let mut wire = *wire;
        let wire_phase = wire.phase.take().unwrap();
        let agent = wire.into_config().unwrap();
        assert_eq!(
            agent.expect,
            Some(crate::check::Check::Contains("ok".into()))
        );
        let rebuilt = wire_phase.into_phase(&agent).unwrap();
        assert_eq!(rebuilt.vars, ["session"]);
        let crate::config::ScenarioAction::Send {
            expect, captures, ..
        } = &rebuilt.actions[0]
        else {
            panic!("not a send");
        };
        assert_eq!(
            expect,
            &Some(crate::check::Check::parse_flag("json:/status=ok").unwrap())
        );
        assert_eq!(captures[0].pointer, "/session");
        // The phase payload reads the variable on the agent too.
        let vars = ["s-1".to_string()];
        let rendered = rebuilt
            .payload
            .as_ref()
            .unwrap()
            .template()
            .unwrap()
            .render(crate::template::Values {
                seq: 0,
                worker_id: 0,
                data: None,
                vars: &vars,
            });
        assert_eq!(rendered, r#"{"op":"sub","s":"s-1"}"#);
    }

    #[cfg(feature = "chaos")]
    #[test]
    fn chaos_travels_with_a_seed_per_agent() {
        use crate::chaos::{ChaosCloseMode, ChaosConfig};
        let chaos = ChaosConfig::new()
            .with_delay(Duration::from_millis(2), Duration::from_millis(3))
            .with_drop(10.0)
            .with_corrupt(5.5)
            .with_close(2.0, ChaosCloseMode::Graceful)
            .with_read_pause(Duration::from_millis(7), Duration::from_millis(1))
            .with_seed(99);
        let whole = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .connections(3)
                .chaos(chaos.clone()),
        );
        let shares = plan(&whole, 3).unwrap();
        let seeds: Vec<u64> = shares
            .iter()
            .map(|share| share.chaos.as_ref().unwrap().seed)
            .collect();
        let expected: Vec<u64> = (0..3).map(|i| crate::chaos::agent_seed(99, i)).collect();
        assert_eq!(seeds, expected);

        let message = Message::Prepare {
            config: Box::new(shares[1].clone()),
            start_timeout: Duration::from_secs(1),
        };
        let Message::Prepare { config: wire, .. } = Message::decode(&message.encode()).unwrap()
        else {
            panic!("not a prepare message");
        };
        let agent = wire.into_config().unwrap();
        assert_eq!(
            agent.chaos,
            Some(chaos.with_seed(crate::chaos::agent_seed(99, 1)))
        );

        let mut bad = shares[0].clone();
        bad.chaos.as_mut().unwrap().drop_percent = 101.0;
        assert!(bad.into_config().is_err());
        let mut listen = shares[0].clone();
        listen.mode = "listen".to_string();
        listen.reply_match = "first".to_string();
        let err = listen.into_config().unwrap_err().to_string();
        assert!(err.contains("chaos injection needs data sends"), "{err}");

        // Totals carry the agent's counts.
        let mut totals = MetricsTotals::new().unwrap();
        totals.chaos = Some(ChaosSummary {
            seed: 5,
            delayed: 1,
            dropped: 2,
            corrupted: 3,
            closes: 4,
            read_pauses: 6,
        });
        let json = serde_json::to_string(&WireTotals::from(&totals)).unwrap();
        let back: WireTotals = serde_json::from_str(&json).unwrap();
        assert_eq!(back.into_totals().unwrap().chaos, totals.chaos);
    }

    #[cfg(all(feature = "chaos", feature = "scenario"))]
    #[test]
    fn a_phase_carries_the_chaos_the_coordinator_resolved() {
        let scenario = crate::ScenarioConfig::from_yaml(
            r#"
version: "1.0"
target: "ws://127.0.0.1:9/"
metadata:
  name: "chaos"
phases:
  - name: "scripted"
    connections: 4
    duration: "1s"
    chaos:
      drop: 25
      seed: 11
    actions:
      - type: Send
        payload: "x"
        count: 2
"#,
        )
        .unwrap();
        let phase = scenario.compile(None).unwrap().remove(0);
        // Listen mode sends nothing itself; the action script does.
        let base = config(
            LoadTestConfig::builder("ws://127.0.0.1:9/")
                .mode(TestMode::Listen)
                .reply_match(ReplyMatch::First),
        );
        let whole = crate::runner::phase_config(base, &phase);
        let mut share = plan(&whole, 2).unwrap().remove(1);
        share.phase = Some(WirePhase::from_phase(&phase));
        let wire_phase = share.phase.clone().unwrap();
        let agent = share.clone().into_config().unwrap();
        let seed = crate::chaos::agent_seed(11, 1);
        assert_eq!(agent.chaos.as_ref().and_then(|c| c.seed), Some(seed));
        let rebuilt = wire_phase.into_phase(&agent).unwrap();
        assert_eq!(rebuilt.chaos, agent.chaos);
        let runs = crate::runner::phase_config(agent.clone(), &rebuilt);
        assert_eq!(runs.chaos, agent.chaos);

        let mut unscripted = share;
        unscripted.phase.as_mut().unwrap().actions.clear();
        assert!(unscripted.into_config().is_err());
    }

    #[test]
    fn results_carry_the_generator_lag_and_the_open_file_limit() {
        let mut hist = crate::lag::lag_histogram().unwrap();
        for micros in [900, 1_000, 1_100, 1_200, 45_000] {
            hist.record(micros).unwrap();
        }
        // Checked against the agent's threshold, which may not be this
        // machine's.
        let lag = LagTotals::from_parts(hist, 4, 1, 36_000);
        let limit = OpenFileLimit {
            soft_limit: 1_024,
            needed: 65,
            sufficient: true,
        };
        let result = WireResult::new(
            &MetricsTotals::new().unwrap(),
            ResultMeta {
                elapsed: Duration::from_secs(1),
                started_at_unix_ms: 0,
                connections: 1,
                not_started: 0,
                capture_dropped: None,
                lag: lag.clone(),
                open_file_limit: Some(limit.clone()),
            },
            TimeSeries::default(),
            &BTreeMap::new(),
        );
        let json = serde_json::to_string(&result).unwrap();
        let decoded = serde_json::from_str::<WireResult>(&json)
            .unwrap()
            .decode()
            .unwrap();
        let summary = decoded.lag.summary().unwrap();
        assert_eq!(Some(&summary), lag.summary().as_ref());
        assert_eq!(summary.samples, 5);
        assert_eq!(summary.threshold_us, 36_000);
        assert_eq!(summary.intervals_over_threshold, 1);
        assert_eq!(decoded.open_file_limit, Some(limit));

        // An agent whose probe took no sample sends none.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["generator_lag"] = serde_json::Value::Null;
        let decoded = serde_json::from_value::<WireResult>(value)
            .unwrap()
            .decode()
            .unwrap();
        assert!(decoded.lag.summary().is_none());
    }

    #[test]
    fn version_2_messages_round_trip() {
        let token = Message::Token {
            value: "Bearer abc".to_string(),
        };
        assert!(matches!(
            Message::decode(&token.encode()).unwrap(),
            Message::Token { value } if value == "Bearer abc"
        ));
        let record = WireRecord {
            t_us: 5,
            worker: 1,
            dir: "recv".to_string(),
            kind: "text".to_string(),
            len: 2,
            code: None,
            data: Some("hi".to_string()),
        };
        let capture = Message::Capture {
            records: vec![record.clone()],
        };
        let encoded = capture.encode();
        assert!(!encoded.contains("code"), "{encoded}");
        assert!(matches!(
            Message::decode(&encoded).unwrap(),
            Message::Capture { records } if records == [record]
        ));
        // A snapshot without intervals still decodes.
        let snapshot = Message::decode(
            r#"{"type":"snapshot","elapsed":{"secs":1,"nanos":0},"live":{"active_connections":1,"connections_established":1,"connections_failed":0,"reconnects":0,"messages_sent":0,"messages_received":0,"bytes_sent":0,"bytes_received":0,"total_errors":0,"unsolicited_frames":0}}"#,
        )
        .unwrap();
        assert!(matches!(snapshot, Message::Snapshot { intervals, .. } if intervals.is_empty()));
        let header = WireHeader {
            name: "authorization".to_string(),
            value: "Bearer secret".to_string(),
        };
        assert!(!format!("{header:?}").contains("secret"));
    }

    #[test]
    fn protocol_versions_must_match() {
        assert!(check_protocol(PROTOCOL_VERSION, "0.0.0").is_ok());
        let err = check_protocol(PROTOCOL_VERSION + 1, "9.9.9").unwrap_err();
        assert!(err.contains("protocol version mismatch"), "{err}");
        assert!(err.contains("9.9.9"), "{err}");
        let Message::Hello { protocol, .. } = Message::decode(&Message::hello().encode()).unwrap()
        else {
            panic!("not a hello");
        };
        assert_eq!(protocol, PROTOCOL_VERSION);
        assert_eq!(
            Message::decode(r#"{"type":"start"}"#).unwrap().kind(),
            "start"
        );
        assert!(Message::decode(r#"{"type":"launch"}"#).is_err());
    }
}
