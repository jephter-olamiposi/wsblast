//! The coordinator: splits a run between agents, starts them together and
//! merges their results, for one run or phase by phase for a scenario.

use super::agent::{RecvError, recv, send};
use super::protocol::{
    AgentResult, Message, PROTOCOL_VERSION, WireConfig, WireResult, WireSnapshot, check_protocol,
    from_sparse, plan, validate_token,
};
use super::ws_config;
use crate::config::LoadTestConfig;
use crate::error::{ConfigError, Result, WsBlastError};
use crate::metrics::{
    AggregatedMetrics, CaptureSummary, DistributedSummary, LiveMetrics, LiveSnapshot, MetricsTotals,
};
use crate::timeseries::{TimeSeries, interval_for, interval_histogram};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hdrhistogram::Histogram;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Default for [`Coordinator::agent_timeout`].
pub const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the coordinator refreshes its live counters, its exported
/// latency and its check for a refreshed authentication token.
const LIVE_TICK: Duration = Duration::from_millis(100);

/// How far the exported latency trails the agents: an interval is exported
/// once it ended this long ago, so every agent's snapshot of it, sent at the
/// agent's next one-second tick, has arrived.
const EXPORT_LAG: Duration = Duration::from_secs(1);

/// Pause between scenario phases, as on a single machine.
#[cfg(feature = "scenario")]
const PHASE_PAUSE: Duration = Duration::from_millis(500);

type Control = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ControlSink = SplitSink<Control, tokio_tungstenite::tungstenite::Message>;
type ControlStream = SplitStream<Control>;

/// An agent as given on the command line, and the URL it is reached at.
#[derive(Debug, Clone)]
struct AgentAddress {
    name: String,
    url: Url,
}

fn parse_agent(raw: &str) -> Result<AgentAddress> {
    let name = raw.trim();
    let invalid = |reason: &str| {
        WsBlastError::from(ConfigError::InvalidOption(format!(
            "invalid agent '{name}': {reason}; use HOST:PORT, ws://HOST:PORT or wss://HOST:PORT"
        )))
    };
    if name.is_empty() {
        return Err(invalid("empty address"));
    }
    let url = if name.contains("://") {
        Url::parse(name).map_err(|e| invalid(&e.to_string()))?
    } else {
        Url::parse(&format!("ws://{name}/")).map_err(|e| invalid(&e.to_string()))?
    };
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(invalid("the scheme must be ws or wss"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("no host"));
    }
    if url.port().is_none() {
        return Err(invalid("no port"));
    }
    Ok(AgentAddress {
        name: name.to_string(),
        url,
    })
}

/// The phases of a scenario, compiled and resolved on the coordinator.
#[cfg(feature = "scenario")]
struct ScenarioPlan {
    name: String,
    /// The scenario's top-level `slo:` thresholds.
    slo: crate::config::SloThresholds,
    phases: Vec<crate::config::ScenarioPhase>,
    targets: Vec<Url>,
}

/// Runs a load test on remote agents started with `wsblast agent`.
///
/// The coordinator generates no load itself. It splits the configuration
/// between the agents, waits until every agent is ready, starts them all at
/// once, follows their progress and merges their results into one
/// [`AggregatedMetrics`], histograms included, so the percentiles are those
/// of every sample. A scenario ([`Coordinator::scenario`]) runs phase by
/// phase, each split and started the same way. docs/distributed.md
/// describes the protocol and METHODOLOGY.md, section 12, the merge.
pub struct Coordinator {
    config: LoadTestConfig,
    #[cfg(feature = "scenario")]
    scenario: Option<ScenarioPlan>,
    agents: Vec<AgentAddress>,
    token: String,
    agent_timeout: Duration,
    agent_ca: Option<PathBuf>,
    progress: bool,
    handle_signals: bool,
    cancel: CancellationToken,
    /// The cluster's live counters, summed from the agents' snapshots.
    live: Arc<LiveMetrics>,
    /// Where the live readers (exporters, TUI) read the latency the
    /// coordinator merges from the agents' interval histograms.
    intervals: Option<Arc<crate::timeseries::LiveIntervals>>,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator")
            .field("agents", &self.agents)
            .field("agent_timeout", &self.agent_timeout)
            .finish_non_exhaustive()
    }
}

/// What a distributed run produced.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DistributedOutcome {
    /// Metrics merged from every agent that sent its result, with
    /// [`AggregatedMetrics::distributed`] listing the agents.
    pub metrics: AggregatedMetrics,
    /// Every agent, in the order given.
    pub agents: Vec<AgentOutcome>,
}

impl DistributedOutcome {
    /// The agents whose result never arrived.
    pub fn lost_agents(&self) -> impl Iterator<Item = &AgentOutcome> {
        self.agents.iter().filter(|agent| agent.metrics.is_none())
    }
}

/// What a distributed scenario produced.
#[cfg(feature = "scenario")]
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DistributedScenarioOutcome {
    /// The scenario's merged metrics, its phases with their merged metrics
    /// and SLO results, and the SLO result of the run, exactly as
    /// [`ScenarioExecutor::run`](crate::ScenarioExecutor::run) reports them
    /// on one machine. `scenario.metrics.distributed` lists the agents and
    /// every agent lost in any phase.
    pub scenario: crate::ScenarioOutcome,
    /// Every agent, in the order given, over the whole scenario: its own
    /// metrics added up over its phases, or, for an agent lost in any phase,
    /// `None` and the reason, prefixed with the phase.
    pub agents: Vec<AgentOutcome>,
    /// For every phase in `scenario.phases`, every agent's part of it.
    pub phases: Vec<Vec<AgentOutcome>>,
}

#[cfg(feature = "scenario")]
impl DistributedScenarioOutcome {
    /// The agents lost in any phase.
    pub fn lost_agents(&self) -> impl Iterator<Item = &AgentOutcome> {
        self.agents.iter().filter(|agent| agent.metrics.is_none())
    }
}

/// One agent's part of a distributed run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentOutcome {
    /// The agent as given.
    pub address: String,
    /// The agent's own metrics, measured over its own run time; `None` when
    /// the agent was lost.
    pub metrics: Option<AggregatedMetrics>,
    /// Why the agent was lost.
    pub error: Option<String>,
    /// The agent's wall clock when it started, in milliseconds since the
    /// Unix epoch, as the agent reported it.
    pub started_at_unix_ms: Option<u64>,
}

/// An open, authenticated control connection that passed the hello.
struct Link {
    sink: ControlSink,
    stream: ControlStream,
    /// Round trip of the hello exchange.
    rtt: Duration,
}

/// The latest live state an agent reported.
#[derive(Default)]
struct AgentLive {
    snapshot: WireSnapshot,
    /// The latency histograms of the phase's recent intervals, replaced by
    /// every snapshot that carries them.
    intervals: BTreeMap<u64, Histogram<u64>>,
}

/// Where a phase's capture records go.
#[cfg(feature = "capture")]
#[derive(Clone)]
struct CaptureSink {
    sender: tokio::sync::mpsc::Sender<crate::capture::MergedBatch>,
    queued: Arc<std::sync::atomic::AtomicU64>,
    offset_us: u64,
}

/// What one agent's control task needs during a phase.
struct Follow {
    index: usize,
    live: Arc<Mutex<Vec<AgentLive>>>,
    stop: watch::Receiver<bool>,
    deadline: tokio::time::Instant,
    agent_timeout: Duration,
    token: Option<watch::Receiver<String>>,
    #[cfg(feature = "capture")]
    capture: Option<CaptureSink>,
}

/// One phase as it is run: the cluster's configuration of it and what the
/// agents add to it.
struct PhaseSpec {
    config: LoadTestConfig,
    wire_phase: Option<super::protocol::WirePhase>,
    /// The phase name in a scenario, prefixed to lost agents' reasons.
    name: Option<String>,
    /// The time series' latency basis, for the live export.
    basis: &'static str,
}

/// One agent's results added up over the phases of a scenario.
#[cfg(feature = "scenario")]
struct AgentTotals {
    totals: MetricsTotals,
    elapsed: Duration,
    connections: u64,
    not_started: u64,
    series: TimeSeries,
}

/// What became of one agent in a phase.
enum Part {
    /// It was lost before the phase, or failed its barrier after the first
    /// phase: it did not run.
    Absent,
    Finished(Box<AgentResult>),
    Lost(String),
}

/// A phase that ran.
struct PhaseRun {
    start: Instant,
    parts: Vec<Part>,
    /// When the last result arrived.
    last_result: Option<Instant>,
    /// The seed of the phase's chaos decisions, which every agent's seed
    /// is derived from; `None` without chaos.
    chaos_seed: Option<u64>,
}

/// The seed of the chaos decisions of a run of `config`.
#[cfg(feature = "chaos")]
fn chaos_seed(config: &LoadTestConfig) -> Option<u64> {
    config.chaos.as_ref().and_then(|chaos| chaos.seed)
}

#[cfg(not(feature = "chaos"))]
fn chaos_seed(_config: &LoadTestConfig) -> Option<u64> {
    None
}

/// The state of a run across its phases.
struct Session {
    links: Vec<Option<Link>>,
    /// Why each agent was lost, once it was.
    lost: Vec<Option<String>>,
    live: Arc<Mutex<Vec<AgentLive>>>,
    #[cfg(feature = "capture")]
    capture: CaptureState,
    /// Capture records the agents reported dropping.
    capture_dropped: u64,
}

#[cfg(feature = "capture")]
enum CaptureState {
    Off,
    /// Created before the run, started with the first phase.
    Pending(crate::capture::CaptureFile),
    Running {
        capture: crate::capture::MergedCapture,
        start: Instant,
    },
}

impl Coordinator {
    /// Creates a coordinator for `config` on `agents`, each `HOST:PORT`
    /// (plain `ws://`), `ws://HOST:PORT` or `wss://HOST:PORT`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid or repeated agent
    /// address, a token shorter than 16 characters, or a configuration that
    /// cannot be split, such as fewer connections than agents.
    pub fn new<I, S>(config: LoadTestConfig, agents: I, token: impl Into<String>) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let token = token.into();
        validate_token(&token)?;
        let agents = parse_agents(agents)?;
        plan(&config, agents.len())?;
        Ok(Self {
            progress: config.progress(),
            config,
            #[cfg(feature = "scenario")]
            scenario: None,
            agents,
            token,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            agent_ca: None,
            handle_signals: true,
            cancel: CancellationToken::new(),
            live: LiveMetrics::new(),
            intervals: None,
        })
    }

    /// Creates a coordinator that runs `scenario` on `agents`, phase by
    /// phase; run it with [`Coordinator::run_scenario`].
    ///
    /// `base_config`, `phase` and `target_override` mean what they mean for
    /// [`ScenarioExecutor::new`](crate::ScenarioExecutor::new). Every phase
    /// is split between the agents as a single run is, so every phase must
    /// have at least as many connections (or highest stage target) as there
    /// are agents.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ScenarioExecutor::new`](crate::ScenarioExecutor::new)
    /// and of [`Coordinator::new`], naming the phase that cannot be split.
    #[cfg(feature = "scenario")]
    #[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
    pub fn scenario<I, S>(
        scenario: &crate::ScenarioConfig,
        base_config: LoadTestConfig,
        phase: Option<&str>,
        target_override: Option<&str>,
        agents: I,
        token: impl Into<String>,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let token = token.into();
        validate_token(&token)?;
        let agents = parse_agents(agents)?;
        scenario.validate()?;
        scenario.validate_slo_mode(base_config.mode, phase)?;
        let plan_ = ScenarioPlan {
            name: scenario.metadata.name.clone(),
            slo: scenario.slo()?,
            targets: scenario.resolve_phase_targets(target_override, phase)?,
            phases: scenario.compile(phase)?,
        };
        for (phase, target) in plan_.phases.iter().zip(&plan_.targets) {
            let mut config = crate::runner::phase_config(base_config.clone(), phase);
            config.target_url = target.clone();
            plan(&config, agents.len()).map_err(|err| match err {
                WsBlastError::Config(ConfigError::InvalidOption(reason)) => WsBlastError::Config(
                    ConfigError::InvalidOption(format!("phase '{}': {reason}", phase.name)),
                ),
                other => other,
            })?;
        }
        Ok(Self {
            progress: base_config.progress(),
            config: base_config,
            scenario: Some(plan_),
            agents,
            token,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            agent_ca: None,
            handle_signals: true,
            cancel: CancellationToken::new(),
            live: LiveMetrics::new(),
            intervals: None,
        })
    }

    /// How long the agents get to connect, authenticate, validate the
    /// configuration and become ready (the start barrier, for every phase of
    /// a scenario), and how long past the run's duration, or past a stop,
    /// each agent gets to send its result. Defaults to 30 seconds.
    #[must_use]
    pub fn agent_timeout(mut self, timeout: Duration) -> Self {
        self.agent_timeout = timeout;
        self
    }

    /// Trusts the CA certificates in this PEM file, in addition to the
    /// platform's roots, for `wss://` agents.
    #[must_use]
    pub fn agent_ca(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_ca = Some(path.into());
        self
    }

    /// Draws a progress bar on stderr while the agents run. Defaults to the
    /// configuration's [`progress`](LoadTestConfig::progress).
    #[must_use]
    pub fn progress(mut self, enabled: bool) -> Self {
        self.progress = enabled;
        self
    }

    /// Controls whether [`Coordinator::run`] stops the agents on Ctrl+C and
    /// SIGTERM. Enabled by default.
    #[must_use]
    pub fn with_signal_handling(mut self, enabled: bool) -> Self {
        self.handle_signals = enabled;
        self
    }

    /// The token that stops the run: before the start nothing runs, after
    /// it every agent is told to stop and the results are still collected.
    /// In a scenario it stops the current phase and skips the others.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The cluster's live counters: the sum of the agents' latest
    /// snapshots, which arrive every second, and across every phase of a
    /// scenario.
    pub fn live_snapshot(&self) -> LiveSnapshot {
        self.live.snapshot()
    }

    /// Keeps the cluster's live counters in `live`, for the exporters and a
    /// display that read them while the run goes on.
    pub(crate) fn with_live_metrics(mut self, live: Arc<LiveMetrics>) -> Self {
        self.live = live;
        self
    }

    /// Publishes the latency merged from the agents to the live readers
    /// (exporters, TUI) of `intervals`.
    pub(crate) fn with_live_intervals(
        mut self,
        intervals: Option<Arc<crate::timeseries::LiveIntervals>>,
    ) -> Self {
        self.intervals = intervals;
        self
    }

    /// Runs the test on every agent and merges the results.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Distributed`] when an agent cannot be
    /// reached, rejects the token or the configuration, or is not ready
    /// within the agent timeout; nothing has run then. Also when the run is
    /// stopped before the agents started, or every agent was lost. An agent
    /// lost after the start is not an error: it is listed in
    /// [`DistributedOutcome::agents`] and in the metrics. A coordinator
    /// created with [`Coordinator::scenario`] returns a configuration error:
    /// run it with [`Coordinator::run_scenario`].
    pub async fn run(&self) -> Result<DistributedOutcome> {
        #[cfg(feature = "scenario")]
        if self.scenario.is_some() {
            return Err(ConfigError::InvalidOption(
                "this coordinator runs a scenario; call run_scenario".to_string(),
            )
            .into());
        }
        let _signals = self.signals();
        let deadline = tokio::time::Instant::now() + self.agent_timeout;
        let mut session = self.open(deadline).await?;
        let spec = PhaseSpec {
            basis: crate::runner::latency_basis_for(&self.config, false),
            config: self.config.clone(),
            wire_phase: None,
            name: None,
        };
        let phase = match self.run_phase(&mut session, &spec, deadline, true).await {
            Ok(phase) => phase,
            Err(err) => {
                session.close().await;
                return Err(err);
            }
        };
        let capture = self.close(session).await;
        let Some(phase) = phase else {
            return Err(WsBlastError::Distributed(
                "interrupted before the agents started; nothing was run".to_string(),
            ));
        };
        let (metrics, agents) = self.merge_phase(&phase)?;
        let summary = self.summary(&agents);
        Ok(DistributedOutcome {
            metrics: metrics.with_distributed(summary).with_capture(capture),
            agents,
        })
    }

    /// Runs every phase of the scenario on the agents, one after another,
    /// and merges each phase's results, then the whole run's.
    ///
    /// Every phase is split, prepared behind its own start barrier and
    /// started as a single run is. Its results are merged into the phase's
    /// metrics, whose SLOs are checked on the merged metrics; the scenario's
    /// SLOs are checked on the metrics merged over every phase, and the
    /// phases' time series are laid end to end. Phases are separated by
    /// 500 ms, as on a single machine. Cancelling
    /// [`Coordinator::cancel_token`] stops the current phase on every agent
    /// and skips the remaining phases.
    ///
    /// An agent lost in a phase takes no part in later phases; its shares of
    /// them are missing from the report, as its share of the phase it was
    /// lost in is. When an agent fails the barrier of a later phase, it is
    /// lost and the phase runs on the others.
    ///
    /// # Errors
    ///
    /// The errors of [`Coordinator::run`] for the first phase; a coordinator
    /// created with [`Coordinator::new`] returns a configuration error.
    #[cfg(feature = "scenario")]
    #[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
    pub async fn run_scenario(&self) -> Result<DistributedScenarioOutcome> {
        let Some(scenario) = &self.scenario else {
            return Err(ConfigError::InvalidOption(
                "this coordinator runs a single test; call run".to_string(),
            )
            .into());
        };
        let _signals = self.signals();
        let deadline = tokio::time::Instant::now() + self.agent_timeout;
        let mut session = self.open(deadline).await?;
        let mut runs: Vec<(usize, PhaseRun)> = Vec::new();
        let mut first = true;
        for (index, (phase, target)) in scenario.phases.iter().zip(&scenario.targets).enumerate() {
            if self.cancel.is_cancelled() || session.links.iter().all(Option::is_none) {
                break;
            }
            let load = if phase.stages.is_empty() {
                format!("{} connections for {:?}", phase.connections, phase.duration)
            } else {
                format!("stages {}", crate::report::stage_list_label(&phase.stages))
            };
            eprintln!(
                "{} Scenario '{}' phase {}/{}: '{}' ({load}) on {} agents",
                if index > 0 { "→" } else { "▶" },
                scenario.name,
                index + 1,
                scenario.phases.len(),
                phase.name,
                session.links.iter().flatten().count(),
            );
            let mut config = crate::runner::phase_config(self.config.clone(), phase);
            config.target_url = target.clone();
            let spec = PhaseSpec {
                basis: crate::runner::latency_basis_for(
                    &config,
                    crate::runner::phase_skips_response_time(phase),
                ),
                config,
                wire_phase: Some(super::protocol::WirePhase::from_phase(phase)),
                name: Some(phase.name.clone()),
            };
            let phase_deadline = if first {
                deadline
            } else {
                tokio::time::Instant::now() + self.agent_timeout
            };
            let ran = match self
                .run_phase(&mut session, &spec, phase_deadline, first)
                .await
            {
                Ok(ran) => ran,
                Err(err) => {
                    session.close().await;
                    return Err(err);
                }
            };
            first = false;
            let Some(ran) = ran else {
                break;
            };
            let any = ran
                .parts
                .iter()
                .any(|part| matches!(part, Part::Finished(_)));
            if any {
                runs.push((index, ran));
            }
            if !any || index + 1 == scenario.phases.len() {
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(PHASE_PAUSE) => {}
                () = self.cancel.cancelled() => {}
            }
        }
        let lost = session.lost.clone();
        let capture = self.close(session).await;
        if runs.is_empty() {
            if lost.iter().all(Option::is_none) {
                return Err(WsBlastError::Distributed(
                    "interrupted before the agents started; nothing was run".to_string(),
                ));
            }
            let reasons: Vec<String> = self
                .agents
                .iter()
                .zip(&lost)
                .map(|(agent, reason)| {
                    format!(
                        "agent {}: {}",
                        agent.name,
                        reason.as_deref().unwrap_or("lost")
                    )
                })
                .collect();
            return Err(WsBlastError::Distributed(format!(
                "every agent was lost, so there is nothing to report: {}",
                reasons.join("; ")
            )));
        }
        self.merge_scenario(scenario, &runs, &lost, capture)
    }

    fn signals(&self) -> Option<crate::runner::SignalListener> {
        if self.handle_signals {
            crate::runner::spawn_signal_listener(self.cancel.clone())
        } else {
            None
        }
    }

    /// Connects to every agent and exchanges hellos, within `deadline`.
    /// Creates the capture file, so a bad path fails before anything runs.
    async fn open(&self, deadline: tokio::time::Instant) -> Result<Session> {
        #[cfg(feature = "capture")]
        let capture = match &self.config.capture {
            Some(capture) => {
                #[cfg(feature = "scenario")]
                let target = self
                    .scenario
                    .as_ref()
                    .and_then(|plan| plan.targets.first())
                    .unwrap_or(&self.config.target_url);
                #[cfg(not(feature = "scenario"))]
                let target = &self.config.target_url;
                CaptureState::Pending(crate::capture::CaptureFile::create(
                    capture,
                    target.as_str(),
                )?)
            }
            None => CaptureState::Off,
        };
        let tls = if self.agents.iter().any(|agent| agent.url.scheme() == "wss") {
            Some(super::tls::client_config(self.agent_ca.as_deref())?)
        } else {
            None
        };
        let connects = futures_util::future::join_all(self.agents.iter().map(|agent| {
            let tls = tls.clone();
            async move {
                tokio::time::timeout_at(deadline, connect(agent, &self.token, tls))
                    .await
                    .unwrap_or_else(|_| Err(self.not_ready()))
            }
        }));
        let links = tokio::select! {
            links = connects => links,
            () = self.cancel.cancelled() => {
                return Err(WsBlastError::Distributed(
                    "interrupted before the agents started; nothing was run".to_string(),
                ));
            }
        };
        let failures = self.failures(links.iter().map(|link| link.as_ref().err()));
        let links: Vec<Option<Link>> = links.into_iter().map(std::result::Result::ok).collect();
        if !failures.is_empty() {
            release(links).await;
            return Err(WsBlastError::Distributed(format!(
                "{}; nothing was run",
                failures.join("; ")
            )));
        }
        Ok(Session {
            lost: vec![None; links.len()],
            live: Arc::new(Mutex::new(
                (0..links.len()).map(|_| AgentLive::default()).collect(),
            )),
            links,
            #[cfg(feature = "capture")]
            capture,
            capture_dropped: 0,
        })
    }

    fn not_ready(&self) -> String {
        format!(
            "not ready within {:?} (--agent-timeout)",
            self.agent_timeout
        )
    }

    /// `agent NAME: reason` for every failure.
    fn failures<'a>(&self, results: impl Iterator<Item = Option<&'a String>>) -> Vec<String> {
        self.agents
            .iter()
            .zip(results)
            .filter_map(|(agent, failure)| {
                failure.map(|reason| format!("agent {}: {reason}", agent.name))
            })
            .collect()
    }

    /// Closes every control connection and the capture file.
    async fn close(&self, session: Session) -> Option<CaptureSummary> {
        #[cfg(feature = "capture")]
        let capture = match session.capture {
            CaptureState::Running { capture, .. } => {
                Some(capture.finish(session.capture_dropped).await)
            }
            CaptureState::Pending(file) => {
                // No phase started: the file holds only what was created.
                let capture = file.start_merged(&self.agent_names()).ok()?.finish(0).await;
                Some(capture)
            }
            CaptureState::Off => None,
        };
        #[cfg(not(feature = "capture"))]
        let capture = None;
        release(session.links).await;
        capture
    }

    #[cfg(feature = "capture")]
    fn agent_names(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.name.clone()).collect()
    }

    /// Prepares the phase on every agent still in the run, starts them
    /// together, follows them and collects their results.
    ///
    /// For the first phase, an agent that fails the barrier, or a stop
    /// before the start, fails the run: nothing has run. For a later phase,
    /// an agent that fails the barrier is lost, and a stop before the start
    /// returns `None`.
    async fn run_phase(
        &self,
        session: &mut Session,
        spec: &PhaseSpec,
        deadline: tokio::time::Instant,
        first: bool,
    ) -> Result<Option<PhaseRun>> {
        let mut shares = plan(&spec.config, self.agents.len())?;
        // Every agent may queue what is left of the limit; the coordinator
        // keeps the limit of the whole run exactly.
        #[cfg(feature = "capture")]
        let capture_limit: Option<Option<u64>> = match &session.capture {
            CaptureState::Off => None,
            CaptureState::Pending(_) => Some(
                self.config
                    .capture
                    .as_ref()
                    .and_then(|capture| (capture.limit != 0).then_some(capture.limit)),
            ),
            CaptureState::Running { capture, .. } => Some(capture.remaining()),
        };
        for share in &mut shares {
            share.phase.clone_from(&spec.wire_phase);
            #[cfg(feature = "capture")]
            if let Some(limit) = capture_limit {
                share.capture = Some(super::protocol::WireCapture { limit });
            }
        }
        let token_value = shares
            .first()
            .and_then(|share| share.live_auth.as_ref())
            .map(|h| h.value.clone());

        // The barrier: every agent still in the run prepares this phase.
        let prepares = futures_util::future::join_all(session.links.iter_mut().zip(shares).map(
            |(link, share)| async move {
                let link = link.as_mut()?;
                Some(
                    tokio::time::timeout_at(deadline, prepare(link, share, self.agent_timeout))
                        .await
                        .unwrap_or_else(|_| Err(self.not_ready())),
                )
            },
        ));
        let prepared = tokio::select! {
            prepared = prepares => prepared,
            () = self.cancel.cancelled() => {
                stop_all(&mut session.links).await;
                if first {
                    return Err(WsBlastError::Distributed(
                        "interrupted before the agents started; nothing was run".to_string(),
                    ));
                }
                return Ok(None);
            }
        };
        let failures = self.failures(
            prepared
                .iter()
                .map(|outcome| outcome.as_ref().and_then(|o| o.as_ref().err())),
        );
        if first && !failures.is_empty() {
            stop_all(&mut session.links).await;
            return Err(WsBlastError::Distributed(format!(
                "{}; nothing was run",
                failures.join("; ")
            )));
        }
        let mut parts: Vec<Part> = Vec::with_capacity(self.agents.len());
        for (index, outcome) in prepared.into_iter().enumerate() {
            match outcome {
                Some(Err(reason)) => {
                    let reason = self.phase_reason(spec, &reason);
                    session.links[index] = None;
                    session.lost[index] = Some(reason.clone());
                    parts.push(Part::Lost(reason));
                }
                Some(Ok(())) => parts.push(Part::Absent),
                None => parts.push(Part::Absent),
            }
        }
        if session.links.iter().all(Option::is_none) {
            return Ok(Some(PhaseRun {
                start: Instant::now(),
                parts,
                last_result: None,
                chaos_seed: chaos_seed(&spec.config),
            }));
        }
        let skew_bound = session
            .links
            .iter()
            .flatten()
            .map(|link| link.rtt)
            .max()
            .unwrap_or_default();

        // Every agent still in the run is ready. The start messages go out
        // concurrently, so the agents start within one control-channel
        // latency of each other.
        let start = Instant::now();
        let sent =
            futures_util::future::join_all(session.links.iter_mut().map(|link| async move {
                match link {
                    Some(link) => Some(send(&mut link.sink, &Message::Start).await),
                    None => None,
                }
            }))
            .await;
        if self.progress {
            eprintln!(
                "{} agents ready, started together (start skew under {:.1} ms, the slowest control round trip)",
                session.links.iter().flatten().count(),
                skew_bound.as_secs_f64() * 1_000.0
            );
        }
        if let Some(intervals) = &self.intervals {
            intervals.begin_remote_phase(start, interval_for(spec.config.duration), spec.basis);
        }
        {
            let mut live = session.live.lock().unwrap_or_else(|e| e.into_inner());
            for agent in live.iter_mut() {
                agent.intervals.clear();
            }
        }
        #[cfg(feature = "capture")]
        let capture_sink = session.start_capture(start, &self.agent_names())?;

        let (stop_tx, stop_rx) = watch::channel(false);
        let (token_tx, token_rx) = watch::channel(token_value.clone().unwrap_or_default());
        let result_deadline =
            tokio::time::Instant::from_std(start + spec.config.duration + self.agent_timeout);
        let mut tasks = Vec::with_capacity(self.agents.len());
        for (index, (link, start_sent)) in session.links.iter_mut().zip(sent).enumerate() {
            let (Some(link), Some(start_sent)) = (link.take(), start_sent) else {
                tasks.push(None);
                continue;
            };
            let follow = Follow {
                index,
                live: Arc::clone(&session.live),
                stop: stop_rx.clone(),
                deadline: result_deadline,
                agent_timeout: self.agent_timeout,
                token: token_value.is_some().then(|| token_rx.clone()),
                #[cfg(feature = "capture")]
                capture: capture_sink.clone(),
            };
            tasks.push(Some(tokio::spawn(async move {
                match start_sent {
                    Ok(()) => follow_agent(link, follow).await,
                    Err(err) => (None, Err(format!("could not be started: {err}"))),
                }
            })));
        }
        drop(stop_rx);
        drop(token_rx);
        #[cfg(feature = "capture")]
        drop(capture_sink);

        let progress = self.progress_bar(spec);
        let all = futures_util::future::join_all(tasks.into_iter().map(|task| async move {
            match task {
                Some(task) => Some(task.await),
                None => None,
            }
        }));
        let mut all = std::pin::pin!(all);
        let mut ticker = tokio::time::interval(LIVE_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stop_sent = false;
        #[cfg(feature = "auth")]
        let mut token_warned = false;
        let interval = interval_for(spec.config.duration);
        let finished = loop {
            tokio::select! {
                finished = &mut all => break finished,
                () = self.cancel.cancelled(), if !stop_sent => {
                    stop_sent = true;
                    let _ = stop_tx.send(true);
                    let message = "Stopping: asking every agent for its result";
                    match &progress {
                        Some(bar) => bar.println(message),
                        None => eprintln!("{message}"),
                    }
                }
                _ = ticker.tick() => {
                    let sum = self.publish_live(&session.live, start, interval);
                    if let Some(bar) = &progress {
                        bar.set_position(start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
                        bar.set_message(format!(
                            "Agents: {} | Active: {} | Sent: {} | Recv: {} | Err: {}",
                            self.agents.len(),
                            sum.active_connections,
                            sum.messages_sent,
                            sum.messages_received,
                            sum.total_errors
                        ));
                    }
                    #[cfg(feature = "auth")]
                    if let (Some(live_auth), Some(_)) = (&spec.config.live_auth, &token_value) {
                        match live_auth.current().to_str() {
                            Ok(value) if *token_tx.borrow() != value => {
                                let _ = token_tx.send(value.to_string());
                            }
                            Ok(_) => {}
                            Err(_) if !token_warned => {
                                token_warned = true;
                                eprintln!(
                                    "Warning: a refreshed authentication header value is not visible ASCII and cannot be sent to the agents; they keep the previous value"
                                );
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
        };
        drop(token_tx);
        if let Some(bar) = progress {
            bar.finish_and_clear();
        }
        self.publish_live(&session.live, start, interval);

        let mut last_result: Option<Instant> = None;
        for (index, joined) in finished.into_iter().enumerate() {
            let Some(joined) = joined else {
                continue;
            };
            let (link, outcome) =
                joined.unwrap_or_else(|e| (None, Err(format!("its control task failed: {e}"))));
            let decoded = outcome.and_then(|(wire, at)| wire.decode().map(|result| (result, at)));
            match decoded {
                Ok((result, at)) => {
                    last_result = Some(last_result.map_or(at, |last| last.max(at)));
                    session.capture_dropped += result.capture_dropped.unwrap_or(0);
                    session.links[index] = link;
                    parts[index] = Part::Finished(Box::new(result));
                }
                Err(reason) => {
                    let reason = self.phase_reason(spec, &reason);
                    session.lost[index] = Some(reason.clone());
                    let mut live = session.live.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(agent) = live.get_mut(index) {
                        agent.snapshot.active_connections = 0;
                    }
                    parts[index] = Part::Lost(reason);
                }
            }
        }
        let run = PhaseRun {
            start,
            parts,
            last_result,
            chaos_seed: chaos_seed(&spec.config),
        };
        if let Some(intervals) = &self.intervals {
            let elapsed = run
                .last_result
                .map_or(Duration::ZERO, |last| last.saturating_duration_since(start));
            if let Some((index, latency)) = final_latency(&run, elapsed, interval) {
                intervals.publish_remote(index, latency);
            }
            intervals.end_phase(elapsed);
        }
        Ok(Some(run))
    }

    fn phase_reason(&self, spec: &PhaseSpec, reason: &str) -> String {
        match &spec.name {
            Some(name) => format!("phase '{name}': {reason}"),
            None => reason.to_string(),
        }
    }

    /// Sums the agents' latest snapshots into the coordinator's live
    /// counters and publishes the latency of the interval that ended
    /// [`EXPORT_LAG`] ago; returns the sum.
    fn publish_live(
        &self,
        live: &Mutex<Vec<AgentLive>>,
        start: Instant,
        interval: Duration,
    ) -> WireSnapshot {
        let agents = live.lock().unwrap_or_else(|e| e.into_inner());
        let mut sum = WireSnapshot::default();
        for agent in agents.iter() {
            sum.add(&agent.snapshot);
        }
        let shard = self.live.shard(0);
        let store = |counter: &std::sync::atomic::AtomicU64, value: u64| {
            counter.store(value, Ordering::Relaxed);
        };
        store(&shard.active_connections, sum.active_connections);
        store(&shard.connections_established, sum.connections_established);
        store(&shard.connections_failed, sum.connections_failed);
        store(&shard.reconnects, sum.reconnects);
        store(&shard.messages_sent, sum.messages_sent);
        store(&shard.messages_received, sum.messages_received);
        store(&shard.bytes_sent, sum.bytes_sent);
        store(&shard.bytes_received, sum.bytes_received);
        store(&shard.total_errors, sum.total_errors);
        store(&shard.unsolicited_frames, sum.unsolicited_frames);

        if let Some(intervals) = &self.intervals {
            let since = start.elapsed().saturating_sub(EXPORT_LAG);
            let latency = crate::timeseries::last_completed(since, interval).map(|bucket| {
                let mut merged: Option<Histogram<u64>> = None;
                for agent in agents.iter() {
                    if let Some(hist) = agent.intervals.get(&bucket) {
                        match merged.as_mut() {
                            Some(merged) => {
                                let _ = merged.add(hist);
                            }
                            None => merged = Some(hist.clone()),
                        }
                    }
                }
                (
                    bucket,
                    crate::timeseries::IntervalLatency::from_histogram(merged.as_ref()),
                )
            });
            if let Some((index, latency)) = latency {
                intervals.publish_remote(index, latency);
            }
        }
        sum
    }

    fn progress_bar(&self, spec: &PhaseSpec) -> Option<ProgressBar> {
        if !self.progress {
            return None;
        }
        let bar =
            ProgressBar::new(spec.config.duration.as_millis().min(u128::from(u64::MAX)) as u64);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% | {msg} | Press Ctrl+C to stop")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        Some(bar)
    }

    /// The agents of the run and the ones lost in it.
    fn summary(&self, agents: &[AgentOutcome]) -> DistributedSummary {
        DistributedSummary {
            agents: agents.iter().map(|agent| agent.address.clone()).collect(),
            lost_agents: agents
                .iter()
                .filter(|agent| agent.metrics.is_none())
                .map(|agent| agent.address.clone())
                .collect(),
        }
    }

    /// Merges the results of one phase: the phase's metrics, measured from
    /// its start to its last result, and every agent's own.
    fn merge_phase(&self, run: &PhaseRun) -> Result<(AggregatedMetrics, Vec<AgentOutcome>)> {
        let mut totals = MetricsTotals::new()?;
        let mut parts = Vec::new();
        let mut connections = 0u64;
        let mut not_started = 0u64;
        let mut agents = Vec::with_capacity(run.parts.len());
        for (address, part) in self.agents.iter().zip(&run.parts) {
            match part {
                Part::Finished(result) => {
                    totals.add(&result.totals)?;
                    connections += result.connections;
                    not_started += result.not_started;
                    let own = result
                        .totals
                        .clone()
                        .finish(result.elapsed, result.connections)
                        .with_not_started(result.not_started)
                        .with_time_series(result.series.series.clone());
                    parts.push(crate::timeseries::SeriesPart {
                        series: result.series.series.clone(),
                        buckets: result.series.buckets.clone(),
                    });
                    agents.push(AgentOutcome {
                        address: address.name.clone(),
                        metrics: Some(own),
                        error: None,
                        started_at_unix_ms: Some(result.started_at_unix_ms),
                    });
                }
                Part::Lost(reason) => agents.push(AgentOutcome {
                    address: address.name.clone(),
                    metrics: None,
                    error: Some(reason.clone()),
                    started_at_unix_ms: None,
                }),
                Part::Absent => agents.push(AgentOutcome {
                    address: address.name.clone(),
                    metrics: None,
                    error: Some("lost in an earlier phase".to_string()),
                    started_at_unix_ms: None,
                }),
            }
        }
        let Some(last_result) = run.last_result else {
            let reasons: Vec<String> = agents
                .iter()
                .map(|agent| {
                    format!(
                        "agent {}: {}",
                        agent.address,
                        agent.error.as_deref().unwrap_or("lost")
                    )
                })
                .collect();
            return Err(WsBlastError::Distributed(format!(
                "every agent was lost, so there is nothing to report: {}",
                reasons.join("; ")
            )));
        };
        let elapsed = last_result.saturating_duration_since(run.start);
        let mut metrics = totals
            .finish(elapsed, connections)
            .with_not_started(not_started)
            .with_time_series(TimeSeries::merge_parallel(parts));
        // The agents' counts add up; their seeds were derived from the
        // run's, which is the one that repeats the run.
        if let (Some(chaos), Some(seed)) = (metrics.chaos.as_mut(), run.chaos_seed) {
            chaos.seed = seed;
        }
        Ok((metrics, agents))
    }

    /// Merges every phase that ran into the scenario's outcome, as
    /// [`ScenarioExecutor::run`](crate::ScenarioExecutor::run) does on one
    /// machine.
    #[cfg(feature = "scenario")]
    fn merge_scenario(
        &self,
        scenario: &ScenarioPlan,
        runs: &[(usize, PhaseRun)],
        lost: &[Option<String>],
        capture: Option<CaptureSummary>,
    ) -> Result<DistributedScenarioOutcome> {
        let scenario_start = runs.first().map_or_else(Instant::now, |(_, run)| run.start);
        let mut totals = MetricsTotals::new()?;
        let mut total_elapsed = Duration::ZERO;
        let mut total_connections = 0u64;
        let mut total_not_started = 0u64;
        let mut time_series = TimeSeries::default();
        let mut phase_results = Vec::with_capacity(runs.len());
        let mut phase_targets = Vec::with_capacity(runs.len());
        let mut phase_agents = Vec::with_capacity(runs.len());
        let mut per_agent: Vec<Option<AgentTotals>> =
            (0..self.agents.len()).map(|_| None).collect();
        let mut started_at: Vec<Option<u64>> = vec![None; self.agents.len()];
        let mut first_chaos_seed: Option<u64> = None;

        for (index, run) in runs {
            let phase = &scenario.phases[*index];
            let target = &scenario.targets[*index];
            let (phase_metrics, agents) = self.merge_phase(run)?;
            if first_chaos_seed.is_none() {
                first_chaos_seed = phase_metrics.chaos.as_ref().map(|chaos| chaos.seed);
            }
            let offset = run.start.saturating_duration_since(scenario_start);
            for (slot, part) in run.parts.iter().enumerate() {
                let Part::Finished(result) = part else {
                    continue;
                };
                totals.add(&result.totals)?;
                started_at[slot].get_or_insert(result.started_at_unix_ms);
                if per_agent[slot].is_none() {
                    per_agent[slot] = Some(AgentTotals {
                        totals: MetricsTotals::new()?,
                        elapsed: Duration::ZERO,
                        connections: 0,
                        not_started: 0,
                        series: TimeSeries::default(),
                    });
                }
                let Some(own) = per_agent[slot].as_mut() else {
                    continue;
                };
                own.totals.add(&result.totals)?;
                own.elapsed += result.elapsed;
                own.connections += result.connections;
                own.not_started += result.not_started;
                own.series.append(result.series.series.clone(), offset);
            }
            total_elapsed += phase_metrics.elapsed;
            total_connections += phase_metrics.total_connections_attempted;
            total_not_started += phase_metrics.total_connections_not_started;
            time_series.append(phase_metrics.time_series.clone(), offset);
            phase_targets.push(crate::report::PhaseTarget {
                phase: phase.name.clone(),
                target_url: target.clone(),
            });
            phase_results.push(crate::runner::PhaseResult {
                name: phase.name.clone(),
                target_url: target.clone(),
                connections: phase.connections,
                stages: phase.stages.clone(),
                slo: crate::report::evaluate_slos(&phase.slo, &phase_metrics),
                metrics: phase_metrics,
            });
            phase_agents.push(agents);
        }

        // An agent lost in any phase is lost for the whole scenario, even
        // when the barrier of a later phase was what it failed.
        let agents: Vec<AgentOutcome> = self
            .agents
            .iter()
            .enumerate()
            .map(
                |(slot, address)| match (&lost[slot], per_agent[slot].take()) {
                    (None, Some(own)) => AgentOutcome {
                        address: address.name.clone(),
                        metrics: Some(
                            own.totals
                                .finish(own.elapsed, own.connections)
                                .with_not_started(own.not_started)
                                .with_time_series(own.series),
                        ),
                        error: None,
                        started_at_unix_ms: started_at[slot],
                    },
                    (reason, _) => AgentOutcome {
                        address: address.name.clone(),
                        metrics: None,
                        error: Some(reason.clone().unwrap_or_else(|| "lost".to_string())),
                        started_at_unix_ms: started_at[slot],
                    },
                },
            )
            .collect();
        let mut metrics = totals
            .finish(total_elapsed, total_connections)
            .with_not_started(total_not_started)
            .with_time_series(time_series)
            .with_capture(capture)
            .with_distributed(self.summary(&agents));
        // As on one machine, the scenario reports the seed of the first
        // phase that injected faults.
        if let (Some(chaos), Some(seed)) = (metrics.chaos.as_mut(), first_chaos_seed) {
            chaos.seed = seed;
        }
        let overall =
            crate::report::evaluate_slos(&self.config.slo.or_from(&scenario.slo), &metrics);
        let slo = crate::report::scenario_slo_evaluation(
            overall,
            phase_results
                .iter()
                .map(|phase| (phase.name.as_str(), &phase.slo)),
        );
        Ok(DistributedScenarioOutcome {
            scenario: crate::ScenarioOutcome {
                metrics,
                phase_targets,
                phases: phase_results,
                slo,
            },
            agents,
            phases: phase_agents,
        })
    }
}

#[cfg(feature = "capture")]
impl Session {
    /// Starts the capture file with the first phase, and returns where this
    /// phase's records go.
    fn start_capture(
        &mut self,
        start: Instant,
        agents: &[String],
    ) -> std::io::Result<Option<CaptureSink>> {
        if let CaptureState::Pending(_) = &self.capture {
            let CaptureState::Pending(file) =
                std::mem::replace(&mut self.capture, CaptureState::Off)
            else {
                return Ok(None);
            };
            self.capture = CaptureState::Running {
                capture: file.start_merged(agents)?,
                start,
            };
        }
        Ok(match &self.capture {
            CaptureState::Running {
                capture,
                start: capture_start,
            } => {
                let (sender, queued) = capture.sink();
                Some(CaptureSink {
                    sender,
                    queued,
                    offset_us: u64::try_from(
                        start.saturating_duration_since(*capture_start).as_micros(),
                    )
                    .unwrap_or(u64::MAX),
                })
            }
            _ => None,
        })
    }
}

impl Session {
    /// Closes every control connection without waiting for a result.
    async fn close(self) {
        release(self.links).await;
    }
}

/// The latency of the last interval that ended before the phase did, from
/// the agents' results.
fn final_latency(
    run: &PhaseRun,
    elapsed: Duration,
    interval: Duration,
) -> Option<(u64, crate::timeseries::IntervalLatency)> {
    let bucket = crate::timeseries::last_completed(elapsed, interval)?;
    let mut merged: Option<Histogram<u64>> = None;
    for part in &run.parts {
        if let Part::Finished(result) = part
            && let Some(hist) = result.series.buckets.get(&bucket)
        {
            match merged.as_mut() {
                Some(merged) => {
                    let _ = merged.add(hist);
                }
                None => merged = Some(hist.clone()),
            }
        }
    }
    Some((
        bucket,
        crate::timeseries::IntervalLatency::from_histogram(merged.as_ref()),
    ))
}

fn parse_agents<I, S>(agents: I) -> Result<Vec<AgentAddress>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let agents = agents
        .into_iter()
        .map(|agent| parse_agent(agent.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    for (i, agent) in agents.iter().enumerate() {
        if agents[..i].iter().any(|other| other.url == agent.url) {
            return Err(ConfigError::InvalidOption(format!(
                "agent '{}' is listed twice; an agent runs one test at a time",
                agent.name
            ))
            .into());
        }
    }
    Ok(agents)
}

/// Tells every agent still connected to stop, without waiting for results.
async fn stop_all(links: &mut [Option<Link>]) {
    let stops = links.iter_mut().flatten().map(|link| async move {
        let _ = send(&mut link.sink, &Message::Stop).await;
    });
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::join_all(stops),
    )
    .await;
}

/// Closes the control connections. An agent that has not sent its last
/// result stops its run.
async fn release(links: Vec<Option<Link>>) {
    let closes = links.into_iter().flatten().map(|mut link| async move {
        let _ = link.sink.close().await;
    });
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::join_all(closes),
    )
    .await;
}

/// Describes why the control connection could not be opened.
fn connect_error(err: WsError) -> String {
    match err {
        WsError::Http(response) => match response.status() {
            http::StatusCode::UNAUTHORIZED => format!(
                "rejected the agent token (HTTP 401); check --agent-token or {}",
                super::TOKEN_ENV
            ),
            http::StatusCode::CONFLICT => {
                "is busy running another test (HTTP 409); wait for it to finish".to_string()
            }
            status => format!("refused the control connection with HTTP {status}"),
        },
        WsError::Io(err) => format!("cannot connect: {err}"),
        WsError::Tls(err) => format!("TLS handshake failed: {err}"),
        other => format!("control connection failed: {other}"),
    }
}

fn reply_error(err: RecvError) -> String {
    match err {
        RecvError::Closed(reason) => format!("closed the control connection ({reason})"),
        RecvError::Invalid(reason) => reason,
    }
}

/// Connects to one agent and checks the protocol version.
async fn connect(
    agent: &AgentAddress,
    token: &str,
    tls: Option<Arc<rustls::ClientConfig>>,
) -> std::result::Result<Link, String> {
    let mut request = agent
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid control URL: {e}"))?;
    let bearer = http::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "the agent token is not a valid header value".to_string())?;
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, bearer);
    let connector = match tls {
        Some(config) if agent.url.scheme() == "wss" => Connector::Rustls(config),
        _ => Connector::Plain,
    };
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(ws_config()),
        true,
        Some(connector),
    )
    .await
    .map_err(connect_error)?;
    let (mut sink, mut stream) = ws.split();

    let sent = Instant::now();
    send(&mut sink, &Message::hello())
        .await
        .map_err(|e| format!("control connection failed: {e}"))?;
    match recv(&mut stream).await.map_err(reply_error)? {
        Message::Hello { protocol, version } => check_protocol(protocol, &version)?,
        Message::Error { message } => return Err(message),
        other => {
            return Err(format!(
                "answered hello with {}; is it a wsblast agent speaking protocol {PROTOCOL_VERSION}?",
                other.kind()
            ));
        }
    }
    Ok(Link {
        sink,
        stream,
        rtt: sent.elapsed(),
    })
}

/// Sends one agent its share and waits for `ready`.
async fn prepare(
    link: &mut Link,
    share: WireConfig,
    start_timeout: Duration,
) -> std::result::Result<(), String> {
    send(
        &mut link.sink,
        &Message::Prepare {
            config: Box::new(share),
            start_timeout,
        },
    )
    .await
    .map_err(|e| format!("control connection failed: {e}"))?;
    match recv(&mut link.stream).await.map_err(reply_error)? {
        Message::Ready { .. } => Ok(()),
        Message::Error { message } => Err(format!("rejected the test: {message}")),
        other => Err(format!("answered prepare with {}", other.kind())),
    }
}

/// Follows one started agent until its result arrives, it disconnects or it
/// misses its deadline. A stop moves the deadline to the agent timeout from
/// the stop if that is sooner. Returns the connection with the result, for
/// the next phase.
async fn follow_agent(
    mut link: Link,
    follow: Follow,
) -> (
    Option<Link>,
    std::result::Result<(WireResult, Instant), String>,
) {
    let Follow {
        index,
        live: agents_live,
        mut stop,
        mut deadline,
        agent_timeout,
        mut token,
        #[cfg(feature = "capture")]
        capture,
    } = follow;
    let mut stopped = false;
    let mut token_open = token.is_some();
    loop {
        tokio::select! {
            received = recv(&mut link.stream) => match received {
                Ok(Message::Snapshot { live, intervals, .. }) => {
                    let mut agents = agents_live.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(agent) = agents.get_mut(index) {
                        agent.snapshot = live;
                        for interval in intervals {
                            let Some(empty) = interval_histogram() else {
                                continue;
                            };
                            if let Ok(hist) = from_sparse(&interval.values, empty) {
                                agent.intervals.insert(interval.index, hist);
                            }
                        }
                        // Only the latest few intervals are exported.
                        while agent.intervals.len() > 4 {
                            agent.intervals.pop_first();
                        }
                    }
                }
                Ok(Message::Capture { records }) => {
                    #[cfg(feature = "capture")]
                    if let Some(capture) = &capture {
                        capture
                            .queued
                            .fetch_add(records.len() as u64, Ordering::Relaxed);
                        let batch = crate::capture::MergedBatch {
                            agent: index,
                            offset_us: capture.offset_us,
                            records,
                        };
                        // Waiting here slows this control channel down, and
                        // with it the agent's forwarding, never its load.
                        let _ = capture.sender.send(batch).await;
                    }
                    #[cfg(not(feature = "capture"))]
                    let _ = records;
                }
                Ok(Message::Result { result }) => {
                    return (Some(link), Ok((*result, Instant::now())));
                }
                Ok(Message::Error { message }) => {
                    return (None, Err(format!("reported an error: {message}")));
                }
                Ok(other) => {
                    return (None, Err(format!("sent an unexpected {} message", other.kind())));
                }
                Err(RecvError::Closed(reason)) => {
                    return (None, Err(format!("disconnected before sending its result ({reason})")));
                }
                Err(RecvError::Invalid(reason)) => return (None, Err(reason)),
            },
            changed = async {
                match token.as_mut() {
                    Some(token) => token.changed().await,
                    None => std::future::pending().await,
                }
            }, if token_open => {
                if changed.is_err() {
                    token_open = false;
                    continue;
                }
                let value = token
                    .as_mut()
                    .map(|token| token.borrow_and_update().clone())
                    .unwrap_or_default();
                let _ = send(&mut link.sink, &Message::Token { value }).await;
            }
            _ = stop.changed(), if !stopped => {
                stopped = true;
                if *stop.borrow() {
                    let _ = send(&mut link.sink, &Message::Stop).await;
                    deadline = deadline.min(tokio::time::Instant::now() + agent_timeout);
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                return (None, Err(format!(
                    "sent no result within {agent_timeout:?} of the end of the run (--agent-timeout)"
                )));
            }
        }
    }
}
