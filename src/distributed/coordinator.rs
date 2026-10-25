//! The coordinator: splits a run between agents, starts them together and
//! merges their results.

use super::agent::{RecvError, recv, send};
use super::protocol::{
    AgentResult, Message, PROTOCOL_VERSION, WireConfig, WireResult, WireSnapshot, check_protocol,
    plan, validate_token,
};
use super::ws_config;
use crate::config::LoadTestConfig;
use crate::error::{ConfigError, Result, WsBlastError};
use crate::metrics::{AggregatedMetrics, DistributedSummary, MetricsTotals};
use crate::timeseries::TimeSeries;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Default for [`Coordinator::agent_timeout`].
pub const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Runs a load test on remote agents started with `wsblast agent`.
///
/// The coordinator generates no load itself. It splits the configuration
/// between the agents, waits until every agent is ready, starts them all at
/// once, follows their progress and merges their results into one
/// [`AggregatedMetrics`], histograms included, so the percentiles are those
/// of every sample. docs/distributed.md describes the protocol and
/// METHODOLOGY.md, section 12, the merge.
pub struct Coordinator {
    config: LoadTestConfig,
    agents: Vec<AgentAddress>,
    token: String,
    agent_timeout: Duration,
    agent_ca: Option<PathBuf>,
    progress: bool,
    handle_signals: bool,
    cancel: CancellationToken,
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

/// An agent that answered `ready`.
struct Prepared {
    sink: ControlSink,
    stream: ControlStream,
    /// Round trip of the hello exchange.
    rtt: Duration,
}

impl Coordinator {
    /// Creates a coordinator for `config` on `agents`, each `HOST:PORT`
    /// (plain `ws://`), `ws://HOST:PORT` or `wss://HOST:PORT`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid or repeated agent
    /// address, a token shorter than 16 characters, a setting distributed
    /// runs do not support (capture, a refreshed auth token, the TUI), or a
    /// configuration that cannot be split, such as fewer connections than
    /// agents.
    pub fn new<I, S>(config: LoadTestConfig, agents: I, token: impl Into<String>) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let token = token.into();
        validate_token(&token)?;
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
        plan(&config, agents.len())?;
        Ok(Self {
            progress: config.progress(),
            config,
            agents,
            token,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            agent_ca: None,
            handle_signals: true,
            cancel: CancellationToken::new(),
        })
    }

    /// How long the agents get to connect, authenticate, validate the
    /// configuration and become ready (the start barrier), and how long past
    /// the run's duration, or past a stop, each agent gets to send its
    /// result. Defaults to 30 seconds.
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
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
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
    /// [`DistributedOutcome::agents`] and in the metrics.
    pub async fn run(&self) -> Result<DistributedOutcome> {
        let _signals = if self.handle_signals {
            crate::runner::spawn_signal_listener(self.cancel.clone())
        } else {
            None
        };
        let shares = plan(&self.config, self.agents.len())?;
        let tls = if self.agents.iter().any(|agent| agent.url.scheme() == "wss") {
            Some(super::tls::client_config(self.agent_ca.as_deref())?)
        } else {
            None
        };

        let deadline = tokio::time::Instant::now() + self.agent_timeout;
        let setups =
            futures_util::future::join_all(self.agents.iter().zip(shares).map(|(agent, share)| {
                let tls = tls.clone();
                async move {
                    tokio::time::timeout_at(
                        deadline,
                        prepare(agent, share, &self.token, tls, self.agent_timeout),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(format!(
                            "not ready within {:?} (--agent-timeout)",
                            self.agent_timeout
                        ))
                    })
                }
            }));
        let setups = tokio::select! {
            setups = setups => setups,
            () = self.cancel.cancelled() => {
                return Err(WsBlastError::Distributed(
                    "interrupted before the agents started; nothing was run".to_string(),
                ));
            }
        };

        let failures: Vec<String> = self
            .agents
            .iter()
            .zip(&setups)
            .filter_map(|(agent, setup)| {
                setup
                    .as_ref()
                    .err()
                    .map(|reason| format!("agent {}: {reason}", agent.name))
            })
            .collect();
        if !failures.is_empty() {
            let releases = setups.into_iter().filter_map(std::result::Result::ok).map(
                |mut prepared| async move {
                    let _ = send(&mut prepared.sink, &Message::Stop).await;
                    let _ = prepared.sink.close().await;
                },
            );
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                futures_util::future::join_all(releases),
            )
            .await;
            return Err(WsBlastError::Distributed(format!(
                "{}; nothing was run",
                failures.join("; ")
            )));
        }
        let mut prepared: Vec<Prepared> = setups.into_iter().filter_map(|s| s.ok()).collect();
        let skew_bound = prepared.iter().map(|p| p.rtt).max().unwrap_or_default();

        // The barrier is passed: every agent is ready. The start messages go
        // out concurrently, so the agents start within one control-channel
        // latency of each other.
        let start = Instant::now();
        let sent = futures_util::future::join_all(
            prepared
                .iter_mut()
                .map(|agent| send(&mut agent.sink, &Message::Start)),
        )
        .await;
        if self.progress {
            eprintln!(
                "{} agents ready, started together (start skew under {:.1} ms, the slowest control round trip)",
                self.agents.len(),
                skew_bound.as_secs_f64() * 1_000.0
            );
        }

        let snapshots = Arc::new(Mutex::new(vec![WireSnapshot::default(); prepared.len()]));
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let result_deadline =
            tokio::time::Instant::from_std(start + self.config.duration + self.agent_timeout);
        let mut tasks = Vec::with_capacity(prepared.len());
        for (index, (agent, start_sent)) in prepared.into_iter().zip(sent).enumerate() {
            let follow = follow(
                agent,
                index,
                Arc::clone(&snapshots),
                stop_rx.clone(),
                result_deadline,
                self.agent_timeout,
            );
            tasks.push(tokio::spawn(async move {
                match start_sent {
                    Ok(()) => follow.await,
                    Err(err) => Err(format!("could not be started: {err}")),
                }
            }));
        }
        drop(stop_rx);

        let progress = self.progress_bar();
        let mut all = std::pin::pin!(futures_util::future::join_all(tasks));
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stop_sent = false;
        let finished = loop {
            tokio::select! {
                finished = &mut all => break finished,
                () = self.cancel.cancelled(), if !stop_sent => {
                    stop_sent = true;
                    let _ = stop_tx.send(true);
                    if let Some(bar) = &progress {
                        bar.println("Stopping: asking every agent for its result");
                    } else {
                        eprintln!("Stopping: asking every agent for its result");
                    }
                }
                _ = ticker.tick() => {
                    if let Some(bar) = &progress {
                        let mut sum = WireSnapshot::default();
                        for snap in snapshots.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                            sum.add(snap);
                        }
                        let elapsed = start.elapsed();
                        bar.set_position(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
                        bar.set_message(format!(
                            "Agents: {} | Active: {} | Sent: {} | Recv: {} | Err: {}",
                            self.agents.len(),
                            sum.active_connections,
                            sum.messages_sent,
                            sum.messages_received,
                            sum.total_errors
                        ));
                    }
                }
            }
        };
        if let Some(bar) = progress {
            bar.finish_and_clear();
        }

        let results = finished.into_iter().map(|joined| {
            joined
                .map_err(|e| format!("its control task failed: {e}"))
                .and_then(|r| r)
                .and_then(|(wire, at)| wire.decode().map(|result| (result, at)))
        });
        self.merge(start, results.collect())
    }

    fn progress_bar(&self) -> Option<ProgressBar> {
        if !self.progress {
            return None;
        }
        let bar =
            ProgressBar::new(self.config.duration.as_millis().min(u128::from(u64::MAX)) as u64);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% | {msg} | Press Ctrl+C to stop")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        Some(bar)
    }

    /// Merges the results that arrived; `start` is when the start messages
    /// were sent.
    fn merge(
        &self,
        start: Instant,
        results: Vec<std::result::Result<(AgentResult, Instant), String>>,
    ) -> Result<DistributedOutcome> {
        let mut totals = MetricsTotals::new()?;
        let mut parts = Vec::new();
        let mut connections = 0u64;
        let mut not_started = 0u64;
        let mut last_result: Option<Instant> = None;
        let mut agents = Vec::with_capacity(results.len());
        for (address, result) in self.agents.iter().zip(results) {
            match result {
                Ok((result, at)) => {
                    totals.add(&result.totals)?;
                    connections += result.connections;
                    not_started += result.not_started;
                    last_result = Some(last_result.map_or(at, |last| last.max(at)));
                    let own = result
                        .totals
                        .finish(result.elapsed, result.connections)
                        .with_not_started(result.not_started)
                        .with_time_series(result.series.series.clone());
                    parts.push(result.series);
                    agents.push(AgentOutcome {
                        address: address.name.clone(),
                        metrics: Some(own),
                        error: None,
                        started_at_unix_ms: Some(result.started_at_unix_ms),
                    });
                }
                Err(reason) => agents.push(AgentOutcome {
                    address: address.name.clone(),
                    metrics: None,
                    error: Some(reason),
                    started_at_unix_ms: None,
                }),
            }
        }
        let Some(last_result) = last_result else {
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
        let summary = DistributedSummary {
            agents: agents.iter().map(|agent| agent.address.clone()).collect(),
            lost_agents: agents
                .iter()
                .filter(|agent| agent.metrics.is_none())
                .map(|agent| agent.address.clone())
                .collect(),
        };
        let elapsed = last_result.saturating_duration_since(start);
        let metrics = totals
            .finish(elapsed, connections)
            .with_not_started(not_started)
            .with_time_series(TimeSeries::merge_parallel(parts))
            .with_distributed(summary);
        Ok(DistributedOutcome { metrics, agents })
    }
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

/// Connects to one agent, checks the protocol version, sends its share and
/// waits for `ready`.
async fn prepare(
    agent: &AgentAddress,
    share: WireConfig,
    token: &str,
    tls: Option<Arc<rustls::ClientConfig>>,
    start_timeout: Duration,
) -> std::result::Result<Prepared, String> {
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
    let rtt = sent.elapsed();

    send(
        &mut sink,
        &Message::Prepare {
            config: Box::new(share),
            start_timeout,
        },
    )
    .await
    .map_err(|e| format!("control connection failed: {e}"))?;
    match recv(&mut stream).await.map_err(reply_error)? {
        Message::Ready { .. } => Ok(Prepared { sink, stream, rtt }),
        Message::Error { message } => Err(format!("rejected the test: {message}")),
        other => Err(format!("answered prepare with {}", other.kind())),
    }
}

/// Follows one started agent until its result arrives, it disconnects or it
/// misses its deadline. A stop moves the deadline to the agent timeout from
/// the stop if that is sooner.
async fn follow(
    mut agent: Prepared,
    index: usize,
    snapshots: Arc<Mutex<Vec<WireSnapshot>>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut deadline: tokio::time::Instant,
    agent_timeout: Duration,
) -> std::result::Result<(WireResult, Instant), String> {
    let mut stopped = false;
    loop {
        tokio::select! {
            received = recv(&mut agent.stream) => match received {
                Ok(Message::Snapshot { live, .. }) => {
                    if let Some(slot) = snapshots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(index)
                    {
                        *slot = live;
                    }
                }
                Ok(Message::Result { result }) => {
                    let at = Instant::now();
                    let _ = tokio::time::timeout(Duration::from_secs(2), agent.sink.close()).await;
                    return Ok((*result, at));
                }
                Ok(Message::Error { message }) => return Err(format!("reported an error: {message}")),
                Ok(other) => return Err(format!("sent an unexpected {} message", other.kind())),
                Err(RecvError::Closed(reason)) => {
                    return Err(format!("disconnected before sending its result ({reason})"));
                }
                Err(RecvError::Invalid(reason)) => return Err(reason),
            },
            _ = stop.changed(), if !stopped => {
                stopped = true;
                if *stop.borrow() {
                    let _ = send(&mut agent.sink, &Message::Stop).await;
                    deadline = deadline.min(tokio::time::Instant::now() + agent_timeout);
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(format!(
                    "sent no result within {agent_timeout:?} of the end of the run (--agent-timeout)"
                ));
            }
        }
    }
}
