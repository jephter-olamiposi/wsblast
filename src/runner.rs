//! `Runner` for one load test and `ScenarioExecutor` for multi-phase
//! scenarios: worker spawning, the connection ramp, run duration, signals and
//! the final metrics merge.

use crate::config::LoadTestConfig;
#[cfg(feature = "scenario")]
use crate::config::{ScenarioConfig, ScenarioPhase};
use crate::connect::ConnectTarget;
use crate::metrics::{AggregatedMetrics, LiveMetrics, LiveSnapshot, WorkerMetrics};
#[cfg(feature = "scenario")]
use crate::report::{PhaseTarget, SloEvaluation};
use crate::timeseries::{
    CounterSampler, IntervalRecorder, IntervalStore, LiveIntervals, TimeSeries, interval_for,
};
use crate::worker::WorkerSession;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The capture handle a run gives its workers; nothing without the
/// `capture` feature.
#[cfg(feature = "capture")]
type SessionCapture = Option<crate::capture::CaptureHandle>;
#[cfg(not(feature = "capture"))]
type SessionCapture = ();

/// A capture file created but not yet started; nothing without the `capture`
/// feature.
#[cfg(feature = "capture")]
type PendingCapture = Option<crate::capture::CaptureFile>;
#[cfg(not(feature = "capture"))]
type PendingCapture = ();

/// How often a staged run adjusts its connection count.
const STAGE_TICK: Duration = Duration::from_millis(10);

/// Runs one load test: opens the configured connections, drives them until
/// the duration or request limit is reached, and merges their metrics.
pub struct Runner {
    config: Arc<LoadTestConfig>,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
    #[cfg(feature = "scenario")]
    scenario: Option<Arc<ScenarioPhase>>,
    /// Where the live readers (exporters, TUI) read the time series of
    /// this run.
    live_intervals: Option<Arc<LiveIntervals>>,
    /// A capture owned by the caller (a scenario records every phase into
    /// one file); otherwise the runner opens its own from the config.
    #[cfg(feature = "capture")]
    capture: Option<crate::capture::CaptureHandle>,
    /// The latency histograms of the run in progress, for a distributed
    /// agent's live snapshots.
    #[cfg(feature = "distributed")]
    intervals: std::sync::Mutex<Option<Arc<IntervalStore>>>,
}

/// The configuration a scenario phase runs with: `config` with the phase's
/// sizing, duration, ramp, stages, rate and payload.
#[cfg(feature = "scenario")]
pub(crate) fn phase_config(mut config: LoadTestConfig, phase: &ScenarioPhase) -> LoadTestConfig {
    config.connections = phase.connections;
    config.duration = phase.duration;
    config.ramp_rate = phase.ramp_rate;
    // A staged phase runs exactly like `--stages`; any other phase holds
    // its connection count, whatever stages the base configuration had.
    config.stages = phase.stages.clone();
    config.rate_per_conn = phase.rate_per_conn;
    if let Some(payload) = phase.payload.clone() {
        config.payload = payload;
    }
    config
}

/// The latency a run of `config` reports in its time series: response time
/// for paced echo runs whose workers record it, RTT otherwise.
/// `skips_response_time` says whether a scenario phase is one that records
/// none (see [`phase_skips_response_time`]).
pub(crate) fn latency_basis_for(
    config: &LoadTestConfig,
    skips_response_time: bool,
) -> &'static str {
    let paced_echo = config.mode == crate::cli::TestMode::Echo && config.rate_per_conn > 0;
    if paced_echo && !skips_response_time {
        "response_time"
    } else {
        "rtt"
    }
}

/// Whether a paced echo phase records no response time: a phase without an
/// action script whose think time pauses after every reply. An action script
/// records response time for every paced send, with its schedule restarted
/// after `Wait` and think time.
#[cfg(feature = "scenario")]
pub(crate) fn phase_skips_response_time(phase: &ScenarioPhase) -> bool {
    phase.actions.is_empty() && !phase.think_time.is_zero()
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
            live_intervals: None,
            #[cfg(feature = "capture")]
            capture: None,
            #[cfg(feature = "distributed")]
            intervals: std::sync::Mutex::new(None),
        }
    }

    /// Creates a runner whose workers execute a compiled scenario phase's
    /// think-time and action script.
    ///
    /// Phase sizing, pacing, and payload override the supplied configuration;
    /// endpoint selection stays with the caller because CLI URLs take
    /// precedence over scenario endpoints.
    #[cfg(feature = "scenario")]
    pub(crate) fn with_scenario(config: LoadTestConfig, phase: ScenarioPhase) -> Self {
        Self {
            config: Arc::new(phase_config(config, &phase)),
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            scenario: Some(Arc::new(phase)),
            live_intervals: None,
            #[cfg(feature = "capture")]
            capture: None,
            #[cfg(feature = "distributed")]
            intervals: std::sync::Mutex::new(None),
        }
    }

    /// The latency the time series reports: response time for paced echo
    /// runs whose workers record it, RTT otherwise.
    fn latency_basis(&self) -> &'static str {
        #[cfg(feature = "scenario")]
        let skips = self
            .scenario
            .as_deref()
            .is_some_and(phase_skips_response_time);
        #[cfg(not(feature = "scenario"))]
        let skips = false;
        latency_basis_for(&self.config, skips)
    }

    fn create_session(
        &self,
        worker_id: usize,
        target: &Arc<ConnectTarget>,
        cancel_token: CancellationToken,
        metrics: WorkerMetrics,
        capture: &SessionCapture,
    ) -> WorkerSession {
        let session = WorkerSession::new(
            worker_id,
            Arc::clone(&self.config),
            Arc::clone(target),
            cancel_token,
            metrics,
        );
        #[cfg(feature = "capture")]
        let session = session.with_capture(capture.clone());
        #[cfg(not(feature = "capture"))]
        let () = *capture;
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

    /// Records this runner's frames into a capture the caller owns and
    /// finishes, instead of one opened from the configuration.
    #[cfg(all(
        feature = "capture",
        any(feature = "scenario", feature = "distributed")
    ))]
    pub(crate) fn with_capture(mut self, capture: Option<crate::capture::CaptureHandle>) -> Self {
        self.capture = capture;
        self
    }

    /// Shares an existing live-metrics registry, allowing the TUI to sample
    /// this runner's telemetry.
    pub(crate) fn with_live_metrics(mut self, live_metrics: Arc<LiveMetrics>) -> Self {
        self.live_metrics = live_metrics;
        self
    }

    /// Publishes this run's time series to the live readers (exporters,
    /// TUI) of `intervals`.
    pub(crate) fn with_live_intervals(mut self, intervals: Option<Arc<LiveIntervals>>) -> Self {
        self.live_intervals = intervals;
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

    /// Runs the load test and returns the merged metrics.
    ///
    /// Workers are started all at once or at the configured ramp rate. The run
    /// ends when the duration (which includes the ramp) elapses, when the
    /// request limit is reached, when [`Runner::cancel_token`] is cancelled,
    /// or on Ctrl+C or SIGTERM unless signal handling is disabled. Every
    /// worker then stops and its metrics are merged, so an early end still
    /// returns everything measured until then. Connection failures are counted
    /// in the metrics, not returned as errors.
    ///
    /// A runner runs once: its token stays cancelled after the run, so create
    /// a new runner for the next run.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Metrics`](crate::WsBlastError::Metrics) if a
    /// latency histogram cannot be created or merged.
    ///
    /// # Examples
    ///
    /// Run in a background task, watch progress, and end the run early:
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use wsblast::{LoadTestConfig, Runner};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> wsblast::Result<()> {
    /// let config = LoadTestConfig::builder("ws://127.0.0.1:9001")
    ///     .duration(Duration::from_secs(60))
    ///     .build()?;
    /// let runner = Arc::new(Runner::new(config).with_signal_handling(false));
    ///
    /// let task = tokio::spawn({
    ///     let runner = Arc::clone(&runner);
    ///     async move { runner.run().await }
    /// });
    ///
    /// tokio::time::sleep(Duration::from_secs(5)).await;
    /// println!("{} replies so far", runner.live_snapshot().messages_received);
    /// runner.cancel_token().cancel();
    ///
    /// let metrics = task.await.expect("runner task panicked")?;
    /// println!("{} messages in {:?}", metrics.total_messages_sent, metrics.elapsed);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(&self) -> crate::error::Result<AggregatedMetrics> {
        let outcome = self.run_and_collect().await?;

        Ok(
            AggregatedMetrics::merge(outcome.worker_metrics, outcome.elapsed, outcome.connections)?
                .with_not_started(outcome.not_started)
                .with_time_series(outcome.time_series)
                .with_capture(outcome.capture),
        )
    }

    /// Waits until the run is cancelled, by its timer, a limit or a signal,
    /// drawing the progress bar meanwhile unless it is disabled.
    async fn wait_until_done(&self, start_time: Instant) {
        if !self.config.no_progress && !self.config.tui {
            let pb = ProgressBar::new(self.config.duration.as_millis() as u64);
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
                pb.set_position(elapsed_ms.min(self.config.duration.as_millis() as u64));

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
    }

    /// Opens and closes connections to follow the configured stages until
    /// the run is cancelled, and returns every worker it started.
    async fn run_stages(
        &self,
        target: &Arc<ConnectTarget>,
        interval_store: &Arc<IntervalStore>,
        start_time: Instant,
        capture: &SessionCapture,
    ) -> Vec<tokio::task::JoinHandle<WorkerMetrics>> {
        let mut open: Vec<(CancellationToken, tokio::task::JoinHandle<WorkerMetrics>)> = Vec::new();
        let mut closed = Vec::new();
        let mut next_id = 0usize;
        let mut tick = tokio::time::interval(STAGE_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break,
                _ = tick.tick() => {}
            }
            let wanted = self.config.stage_target(start_time.elapsed());
            while open.len() < wanted {
                let Ok(metrics) = WorkerMetrics::new(Arc::clone(&self.live_metrics)) else {
                    break;
                };
                let metrics =
                    metrics.with_interval(IntervalRecorder::new(Arc::clone(interval_store)));
                let token = self.cancel_token.child_token();
                let session = self.create_session(next_id, target, token.clone(), metrics, capture);
                open.push((token, tokio::spawn(session.run())));
                next_id += 1;
            }
            // Ramp down by closing the most recently opened connections.
            while open.len() > wanted {
                let Some((token, handle)) = open.pop() else {
                    break;
                };
                token.cancel();
                closed.push(handle);
            }
        }
        closed.extend(open.into_iter().map(|(_, handle)| handle));
        closed
    }

    /// Runs the worker pool and returns the unmerged outcome.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::WsBlastError::Metrics`] if per-worker telemetry
    /// initialization fails.
    pub(crate) async fn run_and_collect(&self) -> crate::error::Result<RunOutcome> {
        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        self.warn_about_limits();

        // Created before the target is resolved so a bad path fails fast.
        #[cfg(feature = "capture")]
        let capture_file = match (&self.capture, &self.config.capture) {
            (None, Some(capture)) => Some(crate::capture::CaptureFile::create(
                capture,
                self.config.target_url.as_str(),
            )?),
            _ => None,
        };
        #[cfg(not(feature = "capture"))]
        let capture_file = ();

        // Resolve the target and load the TLS roots once, before the clock
        // starts, so neither is counted in the run time or handshake latency.
        let target = tokio::select! {
            target = ConnectTarget::prepare(&self.config.target_url, self.config.connect_timeout) => {
                Arc::new(target)
            }
            _ = self.cancel_token.cancelled() => {
                let total_connections = self.config.connections as u64;
                let started_at = Instant::now();
                return Ok(RunOutcome {
                    worker_metrics: Vec::new(),
                    elapsed: Duration::ZERO,
                    connections: total_connections,
                    not_started: total_connections,
                    started_at,
                    time_series: TimeSeries::default(),
                    #[cfg(feature = "distributed")]
                    interval_store: IntervalStore::new(started_at, interval_for(self.config.duration)),
                    capture: None,
                });
            }
        };
        self.drive(target, capture_file).await
    }

    /// Prints the warnings about the ramp and the open-file limit that a run
    /// of this configuration deserves.
    fn warn_about_limits(&self) {
        check_file_descriptor_limit(self.config.connections);
        warn_if_ramp_exceeds_duration(
            self.config.connections,
            self.config.ramp_rate,
            self.config.duration,
        );
    }

    /// Resolves the target and builds the TLS configuration without starting
    /// the run, so a distributed agent can be ready before the coordinator
    /// starts every agent at once.
    #[cfg(feature = "distributed")]
    pub(crate) async fn prepare_target(&self) -> Arc<ConnectTarget> {
        self.warn_about_limits();
        Arc::new(ConnectTarget::prepare(&self.config.target_url, self.config.connect_timeout).await)
    }

    /// The latency histograms of the run in progress, once its clock has
    /// started.
    #[cfg(feature = "distributed")]
    pub(crate) fn interval_store(&self) -> Option<Arc<IntervalStore>> {
        self.intervals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Starts the clock and runs against a target from
    /// [`Runner::prepare_target`]. Signals are not handled here; the caller
    /// cancels the run through [`Runner::cancel_token`].
    #[cfg(feature = "distributed")]
    pub(crate) async fn run_prepared(
        &self,
        target: Arc<ConnectTarget>,
    ) -> crate::error::Result<RunOutcome> {
        #[cfg(feature = "capture")]
        let capture_file = None;
        #[cfg(not(feature = "capture"))]
        let capture_file = ();
        self.drive(target, capture_file).await
    }

    /// Starts the clock, runs the workers until the run is cancelled and
    /// collects their metrics.
    async fn drive(
        &self,
        target: Arc<ConnectTarget>,
        capture_file: PendingCapture,
    ) -> crate::error::Result<RunOutcome> {
        let total_connections = self.config.connections;
        let spawn_delay = if self.config.ramp_rate > 0 {
            Some(Duration::from_secs_f64(1.0 / self.config.ramp_rate as f64))
        } else {
            None
        };
        #[cfg(not(feature = "capture"))]
        let () = capture_file;

        // `--duration` is total wall-clock time including the ramp, so the
        // timer shares `start_time` with elapsed, throughput, and progress.
        let start_time = Instant::now();
        #[cfg(feature = "capture")]
        let own_capture = capture_file
            .map(|file| file.start(start_time))
            .transpose()?;
        #[cfg(feature = "capture")]
        let capture: SessionCapture = self.capture.clone().or_else(|| {
            own_capture
                .as_ref()
                .map(crate::capture::RunningCapture::handle)
        });
        #[cfg(not(feature = "capture"))]
        let capture: SessionCapture = ();
        let interval = interval_for(self.config.duration);
        let interval_store = IntervalStore::new(start_time, interval);
        #[cfg(feature = "distributed")]
        {
            *self.intervals.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Arc::clone(&interval_store));
        }
        if let Some(intervals) = &self.live_intervals {
            intervals.begin_phase(Arc::clone(&interval_store), self.latency_basis());
        }
        let sampler = CounterSampler::spawn(
            Arc::clone(&self.live_metrics),
            start_time,
            interval,
            self.cancel_token.clone(),
        );
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
                    if live.requests_exhausted(max_reqs) {
                        req_token.cancel();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }

        let (handles, connections, not_started) = if self.config.stages.is_empty() {
            let mut handles = Vec::with_capacity(total_connections);
            for worker_id in 0..total_connections {
                if self.cancel_token.is_cancelled() {
                    break;
                }

                let metrics = WorkerMetrics::new(Arc::clone(&self.live_metrics))?
                    .with_interval(IntervalRecorder::new(Arc::clone(&interval_store)));
                handles.push(tokio::spawn(
                    self.create_session(
                        worker_id,
                        &target,
                        self.cancel_token.clone(),
                        metrics,
                        &capture,
                    )
                    .run(),
                ));

                if let Some(delay) = spawn_delay {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = self.cancel_token.cancelled() => break,
                    }
                }
            }
            self.wait_until_done(start_time).await;
            let not_started = (total_connections - handles.len()) as u64;
            (handles, total_connections as u64, not_started)
        } else {
            // Every slot a staged run opens is one it tried, so none is
            // reported as not started.
            let (handles, ()) = tokio::join!(
                self.run_stages(&target, &interval_store, start_time, &capture),
                self.wait_until_done(start_time)
            );
            let opened = handles.len() as u64;
            (handles, opened, 0)
        };

        let elapsed = start_time.elapsed();

        let mut worker_metrics = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(mut metrics) = handle.await {
                metrics.flush_interval();
                worker_metrics.push(metrics);
            }
        }
        if let Some(intervals) = &self.live_intervals {
            intervals.end_phase(elapsed);
        }
        let samples = sampler.finish(elapsed).await;
        let time_series = TimeSeries::build(&samples, &interval_store, self.latency_basis());

        // Every worker has stopped, so the capture is complete; it is
        // flushed and closed before the report is rendered.
        #[cfg(feature = "capture")]
        let capture = match own_capture {
            Some(own) => {
                drop(capture);
                Some(own.finish().await)
            }
            None => None,
        };
        #[cfg(not(feature = "capture"))]
        let capture = None;

        Ok(RunOutcome {
            worker_metrics,
            elapsed,
            connections,
            not_started,
            started_at: start_time,
            time_series,
            #[cfg(feature = "distributed")]
            interval_store,
            capture,
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
    /// When the run's clock started. Scenarios use it to lay their phases'
    /// time series end to end.
    #[cfg_attr(not(feature = "scenario"), allow(dead_code))]
    pub(crate) started_at: Instant,
    pub(crate) time_series: TimeSeries,
    /// The per-interval latency histograms `time_series` was built from,
    /// which a distributed agent sends so its coordinator can merge them.
    #[cfg(feature = "distributed")]
    pub(crate) interval_store: Arc<IntervalStore>,
    /// Totals of the capture this runner opened itself, if any.
    pub(crate) capture: Option<crate::metrics::CaptureSummary>,
}

/// Stops the signal listener task when the owning run ends.
pub(crate) struct SignalListener(tokio::task::JoinHandle<()>);

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
pub(crate) fn spawn_signal_listener(token: CancellationToken) -> Option<SignalListener> {
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
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ScenarioOutcome {
    /// Metrics merged over every executed phase. `elapsed` is the sum of the
    /// phases' run times and the connection counts add up across phases.
    pub metrics: AggregatedMetrics,
    /// Target of every executed phase, in run order. Phases skipped after a
    /// cancellation are not listed.
    pub phase_targets: Vec<PhaseTarget>,
    /// Every executed phase with its own metrics and SLO result, in run
    /// order. Phases skipped after a cancellation are not listed.
    pub phases: Vec<PhaseResult>,
    /// The SLO gate of the whole run: the scenario's top-level `slo:`
    /// thresholds, overridden one by one by those of the base
    /// configuration, checked on [`metrics`](Self::metrics), followed by
    /// every phase's own checks with the phase name prefixed to their names
    /// (`spike: Latency p99`). It passes only when every check passed.
    pub slo: SloEvaluation,
}

#[cfg(feature = "scenario")]
impl ScenarioOutcome {
    /// Whether every SLO of the run passed: the scenario's, the base
    /// configuration's and every phase's.
    pub fn passed(&self) -> bool {
        self.slo.passed
    }
}

/// The result of one executed scenario phase.
#[cfg(feature = "scenario")]
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PhaseResult {
    /// Phase name.
    pub name: String,
    /// Endpoint the phase connected to.
    pub target_url: url::Url,
    /// Configured connection count of the phase; for a staged phase, the
    /// highest stage target.
    pub connections: usize,
    /// The phase's staged load profile; empty for a constant connection
    /// count.
    pub stages: Vec<crate::config::Stage>,
    /// Metrics merged from this phase's workers only. `elapsed` is the
    /// phase's run time, and the time series starts at the phase start.
    pub metrics: AggregatedMetrics,
    /// The phase's own `slo:` thresholds checked on
    /// [`metrics`](Self::metrics), with unprefixed check names; it passes
    /// with no checks when the phase sets none.
    pub slo: SloEvaluation,
}

/// Runs the phases of a [`ScenarioConfig`] one after another.
///
/// Each phase gets its own [`Runner`] with the phase's connections, duration,
/// ramp, rate, payload and actions; the base configuration supplies
/// everything else. Phases are separated by a 500 ms pause.
#[cfg(feature = "scenario")]
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
pub struct ScenarioExecutor {
    scenario_name: String,
    /// The top-level `slo:` thresholds of the scenario file.
    scenario_slo: crate::config::SloThresholds,
    phases: Vec<ScenarioPhase>,
    /// Resolved endpoint of each entry in `phases`.
    targets: Vec<url::Url>,
    base_config: LoadTestConfig,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    handle_signals: bool,
    live_intervals: Option<Arc<LiveIntervals>>,
    /// Where the TUI follows the phases, if it runs.
    progress: Option<Arc<crate::tui::RunProgress>>,
}

#[cfg(feature = "scenario")]
impl ScenarioExecutor {
    /// Builds an executor from a validated scenario and base configuration.
    ///
    /// The base configuration supplies connection-independent settings such as
    /// authentication, headers, timeouts, SLOs, and output preferences. Each
    /// phase overrides sizing, payload, and pacing.
    ///
    /// For each phase the target is `target_override`, then the phase's
    /// `target`, then the scenario's `target`. Set `phase` to run only the
    /// named phase.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidScenario`](crate::ConfigError::InvalidScenario)
    /// when the scenario fails validation, the requested phase is missing, or a
    /// phase has no target from any of the three sources, URL errors for an
    /// unparsable or non-WebSocket target, and
    /// [`ConfigError::UnsupportedOption`](crate::ConfigError::UnsupportedOption)
    /// for a latency SLO in the file when the base configuration's mode is
    /// not echo.
    pub fn new(
        scenario: &ScenarioConfig,
        base_config: LoadTestConfig,
        phase: Option<&str>,
        target_override: Option<&str>,
    ) -> crate::error::Result<Self> {
        scenario.validate()?;
        scenario.validate_slo_mode(base_config.mode, phase)?;
        Ok(Self {
            scenario_name: scenario.metadata.name.clone(),
            scenario_slo: scenario.slo()?,
            targets: scenario.resolve_phase_targets(target_override, phase)?,
            phases: scenario.compile(phase)?,
            base_config,
            live_metrics: LiveMetrics::new(),
            cancel_token: CancellationToken::new(),
            handle_signals: true,
            live_intervals: None,
            progress: None,
        })
    }

    /// Publishes every phase's time series to the live readers (exporters,
    /// TUI) of `intervals`.
    pub(crate) fn with_live_intervals(mut self, intervals: Option<Arc<LiveIntervals>>) -> Self {
        self.live_intervals = intervals;
        self
    }

    /// Reports each phase as it starts and ends to the TUI following
    /// `progress`.
    pub(crate) fn with_progress(mut self, progress: Option<Arc<crate::tui::RunProgress>>) -> Self {
        self.progress = progress;
        self
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
    /// Returns the metrics merged over every phase that ran, each of those
    /// phases with its own metrics, and the SLO result of the run.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Metrics`](crate::WsBlastError::Metrics) if a
    /// latency histogram cannot be created or merged.
    pub async fn run(&self) -> crate::error::Result<ScenarioOutcome> {
        let mut totals = crate::metrics::MetricsTotals::new()?;
        let mut phase_results = Vec::with_capacity(self.phases.len());
        let mut total_elapsed = Duration::ZERO;
        let mut total_connections = 0u64;
        let mut total_not_started = 0u64;
        let mut time_series = TimeSeries::default();
        let scenario_start = Instant::now();
        let mut phase_targets = Vec::with_capacity(self.phases.len());

        let _signal_listener = if self.handle_signals {
            spawn_signal_listener(self.cancel_token.clone())
        } else {
            None
        };

        // One capture file covers every phase, timed from the scenario start.
        #[cfg(feature = "capture")]
        let capture = match (&self.base_config.capture, self.targets.first()) {
            (Some(capture), Some(target)) => Some(
                crate::capture::CaptureFile::create(capture, target.as_str())?
                    .start(scenario_start)?,
            ),
            _ => None,
        };

        for (phase_idx, (phase, target)) in self.phases.iter().zip(&self.targets).enumerate() {
            if self.cancel_token.is_cancelled() {
                break;
            }
            let load = if phase.stages.is_empty() {
                format!("{} connections for {:?}", phase.connections, phase.duration)
            } else {
                format!("stages {}", crate::report::stage_list_label(&phase.stages))
            };
            // The TUI shows the phase itself; a line on stderr would be
            // drawn over it.
            if !self.base_config.tui {
                eprintln!(
                    "{} Scenario '{}' phase {}/{}: '{}' ({load})",
                    if phase_idx > 0 { "→" } else { "▶" },
                    self.scenario_name,
                    phase_idx + 1,
                    self.phases.len(),
                    phase.name,
                );
            }
            if let Some(progress) = &self.progress {
                progress.begin_phase(crate::tui::PhasePlan {
                    name: Some(phase.name.clone()),
                    index: phase_idx,
                    total: self.phases.len(),
                    connections: phase.connections,
                    ramp_rate: phase.ramp_rate,
                    duration: phase.duration,
                    stages: phase.stages.clone(),
                    slo: phase.slo.clone(),
                    run_slo: self.base_config.slo.or_from(&self.scenario_slo),
                });
            }

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
            let runner = runner.with_live_intervals(self.live_intervals.clone());
            #[cfg(feature = "capture")]
            let runner =
                runner.with_capture(capture.as_ref().map(crate::capture::RunningCapture::handle));
            let outcome = runner.run_and_collect().await?;

            total_elapsed += outcome.elapsed;
            total_connections += outcome.connections;
            total_not_started += outcome.not_started;
            // Each phase is summarised from its own workers, and its totals
            // are added to the scenario's, so the merged metrics are those
            // of every worker together.
            let mut phase_totals = crate::metrics::MetricsTotals::new()?;
            for worker in outcome.worker_metrics {
                phase_totals.add_worker(worker);
            }
            totals.add(&phase_totals)?;
            let phase_metrics = phase_totals
                .finish(outcome.elapsed, outcome.connections)
                .with_not_started(outcome.not_started)
                .with_time_series(outcome.time_series.clone());
            let phase_slo = crate::report::evaluate_slos(&phase.slo, &phase_metrics);
            if let Some(progress) = &self.progress {
                progress.end_phase(&phase.name, &phase_slo);
            }
            phase_results.push(PhaseResult {
                name: phase.name.clone(),
                target_url: target.clone(),
                connections: phase.connections,
                stages: phase.stages.clone(),
                slo: phase_slo,
                metrics: phase_metrics,
            });
            time_series.append(
                outcome.time_series,
                outcome.started_at.saturating_duration_since(scenario_start),
            );

            if phase_idx + 1 < self.phases.len() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = self.cancel_token.cancelled() => {}
                }
            }
        }

        #[cfg(feature = "capture")]
        let capture = match capture {
            Some(capture) => Some(capture.finish().await),
            None => None,
        };
        #[cfg(not(feature = "capture"))]
        let capture = None;

        let metrics = totals
            .finish(total_elapsed, total_connections)
            .with_not_started(total_not_started)
            .with_time_series(time_series)
            .with_capture(capture);
        let overall = crate::report::evaluate_slos(
            &self.base_config.slo.or_from(&self.scenario_slo),
            &metrics,
        );
        let slo = crate::report::scenario_slo_evaluation(
            overall,
            phase_results
                .iter()
                .map(|phase| (phase.name.as_str(), &phase.slo)),
        );
        Ok(ScenarioOutcome {
            metrics,
            phase_targets,
            phases: phase_results,
            slo,
        })
    }
}
