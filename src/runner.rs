//! Test execution orchestrator and worker pool management.

use crate::config::LoadTestConfig;
#[cfg(feature = "scenario")]
use crate::config::{ScenarioConfig, ScenarioPhase};
#[cfg(feature = "capture")]
use crate::metrics::FrameCapture;
use crate::metrics::{AggregatedMetrics, LiveMetrics, WorkerMetrics};
use crate::worker::WorkerSession;
#[cfg(unix)]
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Orchestrates the lifecycle of concurrent WebSocket load sessions.
pub struct Runner {
    config: Arc<LoadTestConfig>,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
    #[cfg(feature = "capture")]
    frame_capture: Option<FrameCapture>,
    #[cfg(feature = "scenario")]
    scenario: Option<Arc<ScenarioPhase>>,
}

impl Runner {
    pub fn new(config: LoadTestConfig) -> Self {
        #[cfg(feature = "capture")]
        let frame_capture = config
            .capture
            .enabled
            .then(|| FrameCapture::new(config.capture.max_frames));
        Self {
            config: Arc::new(config),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            #[cfg(feature = "capture")]
            frame_capture,
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
    pub fn with_scenario(mut config: LoadTestConfig, phase: ScenarioPhase) -> Self {
        config.connections = phase.connections;
        config.duration = phase.duration;
        config.ramp_rate = phase.ramp_rate;
        config.rate_per_conn = phase.rate_per_conn;
        if let Some(payload) = phase.payload.clone() {
            config.payload = payload;
        }
        #[cfg(feature = "capture")]
        let frame_capture = config
            .capture
            .enabled
            .then(|| FrameCapture::new(config.capture.max_frames));
        Self {
            config: Arc::new(config),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            #[cfg(feature = "capture")]
            frame_capture,
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
        #[cfg(feature = "capture")]
        let session = session.with_frame_capture(self.frame_capture.clone());
        #[cfg(feature = "scenario")]
        if let Some(phase) = &self.scenario {
            return session.with_scenario(Arc::clone(phase));
        }
        session
    }

    pub fn live_metrics(&self) -> Arc<LiveMetrics> {
        Arc::clone(&self.live_metrics)
    }

    /// Shares an existing live-metrics registry, allowing observability
    /// exporters and the TUI to sample this runner's telemetry.
    pub fn with_live_metrics(mut self, live_metrics: Arc<LiveMetrics>) -> Self {
        self.live_metrics = live_metrics;
        self
    }

    /// Shares a frame buffer so every worker writes into one capture file.
    #[cfg(feature = "capture")]
    pub fn with_frame_capture(mut self, frame_capture: Option<FrameCapture>) -> Self {
        self.frame_capture = frame_capture;
        self
    }

    /// Drains captured frames into the configured file.
    #[cfg(feature = "capture")]
    pub fn write_capture(&self) {
        if let Some(capture) = &self.frame_capture {
            if let Err(err) = capture
                .drain_and_write(&self.config.capture.output_path, self.config.capture.format)
            {
                eprintln!(
                    "Failed to write frame capture to '{}': {err}",
                    self.config.capture.output_path.display()
                );
            }
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Drives this runner from a caller-owned token, typically a child of a
    /// run-level token so that cancelling the parent stops this runner while
    /// this runner's own duration and request limits stay local to it.
    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    /// Controls whether [`Runner::run`] listens for Ctrl+C and SIGTERM.
    ///
    /// Enabled by default. Disable it when the caller owns signal handling,
    /// for example a library host or an orchestrator that cancels a parent of
    /// this runner's token.
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
        let (worker_metrics, elapsed, attempted) = self.run_and_collect_worker_metrics().await?;

        #[cfg(feature = "capture")]
        self.write_capture();

        AggregatedMetrics::merge(worker_metrics, elapsed, attempted)
    }

    /// Runs the worker pool and returns raw worker metrics without merging.
    /// Returns (worker_metrics, elapsed_time, total_connections_attempted).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-worker telemetry
    /// initialization fails.
    pub async fn run_and_collect_worker_metrics(
        &self,
    ) -> crate::error::Result<(Vec<WorkerMetrics>, Duration, u64)> {
        let total_connections = self.config.connections;
        check_file_descriptor_limit(total_connections);

        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        let start_time = Instant::now();
        let spawn_delay = if self.config.ramp_rate > 0 {
            Some(Duration::from_secs_f64(1.0 / self.config.ramp_rate as f64))
        } else {
            None
        };

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

        let duration_token = self.cancel_token.clone();
        let test_duration = self.config.duration;
        tokio::spawn(async move {
            tokio::time::sleep(test_duration).await;
            duration_token.cancel();
        });

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

            while !self.cancel_token.is_cancelled() {
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

                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            pb.finish_and_clear();
        } else {
            self.cancel_token.cancelled().await;
        }

        let elapsed = start_time.elapsed();

        let mut worker_metrics = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(metrics) = handle.await {
                worker_metrics.push(metrics);
            }
        }

        Ok((worker_metrics, elapsed, total_connections as u64))
    }
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

/// Warns if target concurrency approaches or exceeds the process open file descriptor limit.
fn check_file_descriptor_limit(required_conns: usize) {
    #[cfg(unix)]
    {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // SAFETY: getrlimit is invoked with a valid pointer to stack-allocated rlimit struct.
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

/// Orchestrates multi-phase scenario execution with phase transitions.
#[cfg(feature = "scenario")]
pub struct ScenarioExecutor {
    scenario_name: String,
    cli_target: Option<url::Url>,
    scenario_target: Option<url::Url>,
    phases: Vec<ScenarioPhase>,
    base_config: LoadTestConfig,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
    #[cfg(feature = "capture")]
    frame_capture: Option<FrameCapture>,
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
    /// phase is missing or a phase definition fails validation.
    pub fn new(
        scenario: &ScenarioConfig,
        base_config: LoadTestConfig,
        phase: Option<&str>,
        cli_target: Option<&str>,
    ) -> crate::error::Result<Self> {
        #[cfg(feature = "capture")]
        let frame_capture = base_config
            .capture
            .enabled
            .then(|| FrameCapture::new(base_config.capture.max_frames));
        Ok(Self {
            scenario_name: scenario.metadata.name.clone(),
            cli_target: cli_target
                .map(crate::config::parse_target_url)
                .transpose()?,
            scenario_target: scenario
                .target
                .as_deref()
                .map(crate::config::parse_target_url)
                .transpose()?,
            phases: scenario.compile(phase)?,
            base_config,
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            #[cfg(feature = "capture")]
            frame_capture,
        })
    }

    /// Shares an existing live-metrics registry across every phase.
    pub fn with_live_metrics(mut self, live_metrics: Arc<LiveMetrics>) -> Self {
        self.live_metrics = live_metrics;
        self
    }

    /// Returns the registry shared by all phases.
    pub fn live_metrics(&self) -> Arc<LiveMetrics> {
        Arc::clone(&self.live_metrics)
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
    pub fn with_signal_handling(mut self, enabled: bool) -> Self {
        self.handle_signals = enabled;
        self
    }

    /// Executes every compiled phase sequentially until the scenario ends or
    /// the run-level token is cancelled.
    ///
    /// Returns merged worker metrics, total elapsed time, and total connection
    /// attempts across all phases.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-phase telemetry
    /// initialization fails.
    pub async fn run(&self) -> crate::error::Result<(Vec<WorkerMetrics>, Duration, u64)> {
        let mut all_worker_metrics = Vec::new();
        let mut total_elapsed = Duration::ZERO;
        let mut total_connections = 0u64;

        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        for (phase_idx, phase) in self.phases.iter().enumerate() {
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
            if let Some(target) = self
                .cli_target
                .clone()
                .or_else(|| phase.target.clone())
                .or_else(|| self.scenario_target.clone())
            {
                config.target_url = target;
            }

            let runner = Runner::with_scenario(config, phase.clone())
                .with_live_metrics(Arc::clone(&self.live_metrics))
                .with_cancel_token(self.cancel_token.child_token())
                .with_signal_handling(false);
            #[cfg(feature = "capture")]
            let runner = runner.with_frame_capture(self.frame_capture.clone());
            let (worker_metrics, elapsed, connections) =
                runner.run_and_collect_worker_metrics().await?;

            total_elapsed += elapsed;
            total_connections += connections;
            all_worker_metrics.extend(worker_metrics);

            if phase_idx + 1 < self.phases.len() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = self.cancel_token.cancelled() => {}
                }
            }
        }

        #[cfg(feature = "capture")]
        if let Some(capture) = &self.frame_capture {
            if let Err(err) = capture.drain_and_write(
                &self.base_config.capture.output_path,
                self.base_config.capture.format,
            ) {
                eprintln!(
                    "Failed to write frame capture to '{}': {err}",
                    self.base_config.capture.output_path.display()
                );
            }
        }

        Ok((all_worker_metrics, total_elapsed, total_connections))
    }
}
