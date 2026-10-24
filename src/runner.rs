//! Test execution orchestrator and worker pool management.

use crate::config::LoadTestConfig;
#[cfg(feature = "scenario")]
use crate::config::{ScenarioConfig, ScenarioPhase};
use crate::metrics::{AggregatedMetrics, LiveMetrics, LiveSnapshot, WorkerMetrics};
#[cfg(feature = "scenario")]
use crate::report::PhaseTarget;
use crate::worker::WorkerSession;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Runs one load test: opens the configured connections, drives them until
/// the duration or request limit is reached, and merges their metrics.
pub struct Runner {
    config: Arc<LoadTestConfig>,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
    #[cfg(feature = "scenario")]
    scenario: Option<Arc<ScenarioPhase>>,
}

impl Runner {
    /// Creates a runner for `config`. Nothing connects until [`Runner::run`].
    pub fn new(config: LoadTestConfig) -> Self {
        Self {
            config: Arc::new(config),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            #[cfg(feature = "scenario")]
            scenario: None,
        }
    }

    /// Creates a runner whose workers execute a compiled scenario phase's
    /// think-time and action script.
    ///
    /// Phase sizing, pacing, and payload override the supplied configuration;
    /// endpoint selection stays with the caller because CLI URLs take
    /// precedence over scenario endpoints.
    #[cfg(feature = "scenario")]
    pub(crate) fn with_scenario(mut config: LoadTestConfig, phase: ScenarioPhase) -> Self {
        config.connections = phase.connections;
        config.duration = phase.duration;
        config.ramp_rate = phase.ramp_rate;
        config.rate_per_conn = phase.rate_per_conn;
        if let Some(payload) = phase.payload.clone() {
            config.payload = payload;
        }
        Self {
            config: Arc::new(config),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            scenario: Some(Arc::new(phase)),
        }
    }

    fn create_session(&self, worker_id: usize, metrics: WorkerMetrics) -> WorkerSession {
        let session = WorkerSession::new(
            worker_id,
            Arc::clone(&self.config),
            self.cancel_token.clone(),
            metrics,
        );
        #[cfg(feature = "scenario")]
        if let Some(phase) = &self.scenario {
            return session.with_scenario(Arc::clone(phase));
        }
        session
    }

    /// Reads the live counters of this runner, for progress display while
    /// [`Runner::run`] is in progress.
    pub fn live_snapshot(&self) -> LiveSnapshot {
        self.live_metrics.snapshot()
    }

    /// Shares an existing live-metrics registry, allowing the TUI to sample
    /// this runner's telemetry.
    pub(crate) fn with_live_metrics(mut self, live_metrics: Arc<LiveMetrics>) -> Self {
        self.live_metrics = live_metrics;
        self
    }

    /// Returns the token that stops this runner. Cancelling it ends the run
    /// early; [`Runner::run`] still returns the metrics collected so far.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Drives this runner from a caller-owned token, typically a child of a
    /// run-level token so that cancelling the parent stops this runner while
    /// this runner's own duration and request limits stay local to it.
    #[cfg(feature = "scenario")]
    pub(crate) fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    /// Controls whether [`Runner::run`] listens for Ctrl+C and SIGTERM.
    ///
    /// Enabled by default. Disable it when the caller owns signal handling,
    /// for example a library host or an orchestrator that cancels a parent of
    /// this runner's token.
    #[must_use]
    pub fn with_signal_handling(mut self, enabled: bool) -> Self {
        self.handle_signals = enabled;
        self
    }

    /// Spawns the worker pool, tracks real-time progress, and aggregates final metrics.
    ///
    /// Cancellation via timeout or interrupt signal gracefully stops all worker loops
    /// and ensures all in-flight session metrics are aggregated without data loss.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-worker telemetry
    /// initialization or histogram aggregation fails.
    pub async fn run(&self) -> crate::error::Result<AggregatedMetrics> {
        let outcome = self.run_and_collect().await?;

        Ok(
            AggregatedMetrics::merge(outcome.worker_metrics, outcome.elapsed, outcome.connections)?
                .with_not_started(outcome.not_started),
        )
    }

    /// Runs the worker pool and returns the unmerged outcome.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-worker telemetry
    /// initialization fails.
    pub(crate) async fn run_and_collect(&self) -> crate::error::Result<RunOutcome> {
        let total_connections = self.config.connections;
        check_file_descriptor_limit(total_connections);

        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        warn_if_ramp_exceeds_duration(
            total_connections,
            self.config.ramp_rate,
            self.config.duration,
        );

        let spawn_delay = if self.config.ramp_rate > 0 {
            Some(Duration::from_secs_f64(1.0 / self.config.ramp_rate as f64))
        } else {
            None
        };

        // `--duration` is total wall-clock time including the ramp, so the
        // timer shares `start_time` with elapsed, throughput, and progress.
        let start_time = Instant::now();
        let duration_token = self.cancel_token.clone();
        let test_duration = self.config.duration;
        tokio::spawn(async move {
            tokio::time::sleep(test_duration).await;
            duration_token.cancel();
        });

        let mut handles = Vec::with_capacity(total_connections);
        for worker_id in 0..total_connections {
            if self.cancel_token.is_cancelled() {
                break;
            }

            let metrics = WorkerMetrics::new(Arc::clone(&self.live_metrics))?;
            handles.push(tokio::spawn(self.create_session(worker_id, metrics).run()));

            if let Some(delay) = spawn_delay {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = self.cancel_token.cancelled() => break,
                }
            }
        }

        if let Some(max_reqs) = self.config.max_requests {
            let req_token = self.cancel_token.clone();
            let live = Arc::clone(&self.live_metrics);
            tokio::spawn(async move {
                while !req_token.is_cancelled() {
                    if live
                        .messages_sent
                        .load(std::sync::atomic::Ordering::Relaxed)
                        >= max_reqs
                    {
                        req_token.cancel();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }

        if !self.config.no_progress && !self.config.tui {
            let pb = ProgressBar::new(test_duration.as_millis() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% | Conns: {msg} | Press Ctrl+C to stop")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("#>-"),
            );

            // The refresh tick races cancellation so the run ends, and
            // `elapsed` is taken, when the token fires rather than at the
            // next refresh.
            let mut refresh = tokio::time::interval(Duration::from_millis(100));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => break,
                    _ = refresh.tick() => {}
                }
                let snap = self.live_metrics.snapshot();
                let elapsed = start_time.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;
                pb.set_position(elapsed_ms.min(test_duration.as_millis() as u64));

                let rps = if elapsed.as_secs_f64() > 0.0 {
                    snap.messages_received as f64 / elapsed.as_secs_f64()
                } else {
                    0.0
                };

                pb.set_message(format!(
                    "Active: {} | Msg/s: {:.1} | Recv: {} | Err: {}",
                    snap.active_connections, rps, snap.messages_received, snap.total_errors
                ));
            }

            pb.finish_and_clear();
        } else {
            self.cancel_token.cancelled().await;
        }

        let elapsed = start_time.elapsed();
        let not_started = (total_connections - handles.len()) as u64;

        let mut worker_metrics = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(metrics) = handle.await {
                worker_metrics.push(metrics);
            }
        }

        Ok(RunOutcome {
            worker_metrics,
            elapsed,
            connections: total_connections as u64,
            not_started,
        })
    }
}

/// Raw result of one runner pass, before the worker metrics are merged.
#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub(crate) worker_metrics: Vec<WorkerMetrics>,
    pub(crate) elapsed: Duration,
    /// Connection slots the run was configured to open.
    pub(crate) connections: u64,
    /// Slots the ramp had not reached when the run ended. They have no
    /// worker metrics and are neither established nor failed.
    pub(crate) not_started: u64,
}

/// Stops the signal listener task when the owning run ends.
struct SignalListener(tokio::task::JoinHandle<()>);

impl Drop for SignalListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Cancels `token` on Ctrl+C, and on SIGTERM where the platform has it.
///
/// OS handlers are registered before this returns, not when the spawned task
/// is first polled, so a signal delivered while workers are still ramping is
/// observed instead of killing the process with the default disposition. The
/// listener exits once `token` is cancelled or the returned guard is dropped.
fn spawn_signal_listener(token: CancellationToken) -> Option<SignalListener> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = recv_or_pending(sigint.as_mut()) => token.cancel(),
                _ = recv_or_pending(sigterm.as_mut()) => token.cancel(),
                _ = token.cancelled() => {}
            }
        });
        Some(SignalListener(handle))
    }

    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c().ok();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = async {
                    match ctrl_c.as_mut() {
                        Some(ctrl_c) => {
                            ctrl_c.recv().await;
                        }
                        None => std::future::pending().await,
                    }
                } => token.cancel(),
                _ = token.cancelled() => {}
            }
        });
        Some(SignalListener(handle))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = token;
        None
    }
}

#[cfg(unix)]
async fn recv_or_pending(signal: Option<&mut tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// Warns when the ramp cannot open every connection before `duration` ends.
///
/// Workers are spawned at `0, 1/rate, 2/rate, ...`, so the last one starts at
/// `(connections - 1) / rate`; at or past `duration` the run is cancelled first.
fn warn_if_ramp_exceeds_duration(connections: usize, ramp_rate: u64, duration: Duration) {
    if ramp_rate == 0 || connections == 0 {
        return;
    }
    let last_spawn_secs = (connections - 1) as f64 / ramp_rate as f64;
    if last_spawn_secs >= duration.as_secs_f64() {
        let reachable = (duration.as_secs_f64() * ramp_rate as f64).ceil() as usize;
        eprintln!(
            "{} Opening {} connections at --ramp-rate {}/s takes {:.1}s, but the {:.1}s duration includes the ramp; only about {} connections will be opened.",
            colored::Colorize::bold("Warning:").yellow(),
            connections,
            ramp_rate,
            last_spawn_secs,
            duration.as_secs_f64(),
            reachable.min(connections)
        );
    }
}

/// Warns if target concurrency approaches or exceeds the process open file descriptor limit.
fn check_file_descriptor_limit(required_conns: usize) {
    #[cfg(unix)]
    {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // SAFETY: `rlim` is a valid, writable `rlimit` on this stack frame and
        // outlives the call; getrlimit only writes into it and has no other
        // preconditions.
        #[allow(unsafe_code)]
        let res = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
        if res == 0 {
            let soft_limit = rlim.rlim_cur as usize;
            // 64 descriptors reserved for runtime I/O, event loops, and DNS resolution.
            if required_conns.saturating_add(64) > soft_limit {
                eprintln!(
                    "{} Configured connections ({}) approach or exceed system open file limit (ulimit -n = {}). Consider running 'ulimit -n 65535' to avoid EMFILE socket drops.",
                    colored::Colorize::bold("Warning:").yellow(),
                    required_conns,
                    soft_limit
                );
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = required_conns;
    }
}

/// Result of a scenario run.
#[cfg(feature = "scenario")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ScenarioOutcome {
    /// Metrics merged over every executed phase. `elapsed` is the sum of the
    /// phases' run times and the connection counts add up across phases.
    pub metrics: AggregatedMetrics,
    /// Target of every executed phase, in run order. Phases skipped after a
    /// cancellation are not listed.
    pub phase_targets: Vec<PhaseTarget>,
}

/// Runs the phases of a [`ScenarioConfig`] one after another.
#[cfg(feature = "scenario")]
pub struct ScenarioExecutor {
    scenario_name: String,
    phases: Vec<ScenarioPhase>,
    /// Resolved endpoint of each entry in `phases`.
    targets: Vec<url::Url>,
    base_config: LoadTestConfig,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
}

#[cfg(feature = "scenario")]
impl ScenarioExecutor {
    /// Builds an executor from a validated scenario and base configuration.
    ///
    /// The base configuration supplies connection-independent settings such as
    /// authentication, headers, timeouts, SLOs, and output preferences. Each
    /// phase overrides sizing, payload, and pacing.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ConfigError::InvalidScenario`] when the requested
    /// phase is missing, a phase definition fails validation, or a phase has
    /// no target from the command line, the phase, or the scenario.
    pub fn new(
        scenario: &ScenarioConfig,
        base_config: LoadTestConfig,
        phase: Option<&str>,
        target_override: Option<&str>,
    ) -> crate::error::Result<Self> {
        scenario.validate()?;
        Ok(Self {
            scenario_name: scenario.metadata.name.clone(),
            targets: scenario.resolve_phase_targets(target_override, phase)?,
            phases: scenario.compile(phase)?,
            base_config,
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
        })
    }

    /// Shares an existing live-metrics registry across every phase.
    pub(crate) fn with_live_metrics(mut self, live_metrics: Arc<LiveMetrics>) -> Self {
        self.live_metrics = live_metrics;
        self
    }

    /// Reads the live counters, summed over the phases run so far.
    pub fn live_snapshot(&self) -> LiveSnapshot {
        self.live_metrics.snapshot()
    }

    /// Returns the run-level token. Cancelling it stops the current phase
    /// and skips every remaining phase; each phase runs on a child of it.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Controls whether [`ScenarioExecutor::run`] listens for Ctrl+C and
    /// SIGTERM. Enabled by default; phase runners never install their own
    /// listener, so with this disabled only [`ScenarioExecutor::cancel_token`]
    /// stops the scenario early.
    #[must_use]
    pub fn with_signal_handling(mut self, enabled: bool) -> Self {
        self.handle_signals = enabled;
        self
    }

    /// Executes every compiled phase sequentially until the scenario ends or
    /// the run-level token is cancelled.
    ///
    /// Returns the metrics merged over every phase that ran and the target
    /// of each of those phases.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-phase telemetry
    /// initialization fails.
    pub async fn run(&self) -> crate::error::Result<ScenarioOutcome> {
        let mut all_worker_metrics = Vec::new();
        let mut total_elapsed = Duration::ZERO;
        let mut total_connections = 0u64;
        let mut total_not_started = 0u64;
        let mut phase_targets = Vec::with_capacity(self.phases.len());

        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        for (phase_idx, (phase, target)) in self.phases.iter().zip(&self.targets).enumerate() {
            if self.cancel_token.is_cancelled() {
                break;
            }
            eprintln!(
                "{} Scenario '{}' phase {}/{}: '{}' ({} connections for {:?})",
                if phase_idx > 0 { "→" } else { "▶" },
                self.scenario_name,
                phase_idx + 1,
                self.phases.len(),
                phase.name,
                phase.connections,
                phase.duration
            );

            let mut config = self.base_config.clone();
            config.target_url = target.clone();
            phase_targets.push(PhaseTarget {
                phase: phase.name.clone(),
                target_url: target.clone(),
            });

            let runner = Runner::with_scenario(config, phase.clone())
                .with_live_metrics(Arc::clone(&self.live_metrics))
                .with_cancel_token(self.cancel_token.child_token())
                .with_signal_handling(false);
            let outcome = runner.run_and_collect().await?;

            total_elapsed += outcome.elapsed;
            total_connections += outcome.connections;
            total_not_started += outcome.not_started;
            all_worker_metrics.extend(outcome.worker_metrics);

            if phase_idx + 1 < self.phases.len() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = self.cancel_token.cancelled() => {}
                }
            }
        }

        let metrics =
            AggregatedMetrics::merge(all_worker_metrics, total_elapsed, total_connections)?
                .with_not_started(total_not_started);
        Ok(ScenarioOutcome {
            metrics,
            phase_targets,
        })
    }
}
