//! What the dashboard renders from.
//!
//! The dashboard never touches workers or histograms directly. Each refresh
//! it asks a [`DashboardSource`] for four things: the live counters, the
//! run's and the current phase's clocks, the latency of the most recently
//! completed time-series interval, and the plan and results of the phases
//! ([`ProgressView`]). Every one of these reads is cheap and lock-light, so
//! the dashboard can refresh at its own rate without slowing the
//! measurement path.
//!
//! # Driving the dashboard from somewhere else
//!
//! A local run uses [`RunFeed`], which reads the run's [`LiveMetrics`], the
//! [`LiveIntervals`] the runner registers each phase's interval store in,
//! and the [`RunProgress`] the scenario executor (or the caller, for a single
//! run) keeps up to date.
//!
//! The coordinator of a distributed run (`--agents`) can drive the same
//! dashboard without any change to it, in either of two ways:
//!
//! - Build a [`RunFeed`] from its own parts: add the agents' counter deltas
//!   into a [`LiveMetrics`] shard as their progress messages arrive, merge
//!   their per-interval histograms into an
//!   [`IntervalStore`](crate::timeseries::IntervalStore) registered once with
//!   [`LiveIntervals::begin_phase`] (and ended with
//!   [`LiveIntervals::end_phase`]), and give [`RunProgress::begin_phase`] the
//!   run's [`PhasePlan::for_run`].
//! - Or implement [`DashboardSource`] directly on whatever state it already
//!   keeps, as the tests of this module do.
//!
//! Either way, it then passes the source to [`TuiApp`](super::TuiApp).

use crate::config::{LoadTestConfig, SloThresholds, Stage};
use crate::metrics::{LiveMetrics, LiveSnapshot};
use crate::report::SloEvaluation;
use crate::timeseries::{CompletedInterval, LiveIntervals, PhaseClock};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Everything the dashboard reads on each refresh. Implementations must be
/// cheap to call several times a second and must not block for long.
pub(crate) trait DashboardSource: Send + Sync {
    /// Live counters, summed over every worker (for a coordinator, over
    /// every agent).
    fn counters(&self) -> LiveSnapshot;

    /// Time since the run's clock started; zero before it starts.
    fn run_elapsed(&self) -> Duration;

    /// The clock of the run or scenario phase in progress; `None` before
    /// the first one starts.
    fn phase_clock(&self) -> Option<PhaseClock>;

    /// Latency of the most recently completed interval of the phase in
    /// progress; `None` before its first interval ends.
    fn last_interval(&self) -> Option<CompletedInterval>;

    /// The phase in progress and the phases that have finished.
    fn progress(&self) -> ProgressView;
}

/// What one run or scenario phase was configured to do.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PhasePlan {
    /// Phase name; `None` for a single run.
    pub(crate) name: Option<String>,
    /// 0-based position of the phase in the run.
    pub(crate) index: usize,
    /// Number of phases in the run; 1 for a single run.
    pub(crate) total: usize,
    /// Configured connections; for a staged phase, the highest target.
    pub(crate) connections: usize,
    /// Workers started per second; 0 starts them all at once.
    pub(crate) ramp_rate: u64,
    /// Planned length of the phase.
    pub(crate) duration: Duration,
    /// Staged load profile; empty for a constant connection count.
    pub(crate) stages: Vec<Stage>,
    /// The thresholds checked on this phase alone: a scenario phase's own
    /// `slo:`, or the SLO flags of a single run.
    pub(crate) slo: SloThresholds,
    /// The thresholds checked on the whole scenario; empty for a single run,
    /// whose `slo` already covers the whole run.
    pub(crate) run_slo: SloThresholds,
}

impl PhasePlan {
    /// The plan of a single run of `config`.
    pub(crate) fn for_run(config: &LoadTestConfig) -> Self {
        Self {
            name: None,
            index: 0,
            total: 1,
            connections: config.connections,
            ramp_rate: config.ramp_rate,
            duration: config.duration,
            stages: config.stages.clone(),
            slo: config.slo.clone(),
            run_slo: SloThresholds::default(),
        }
    }
}

/// The SLO result of a finished phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinishedPhase {
    pub(crate) name: String,
    /// Whether every check of the phase's own SLOs passed; true when it has
    /// none.
    pub(crate) passed: bool,
    /// Number of checks evaluated.
    pub(crate) checks: usize,
    /// Names of the checks that failed.
    pub(crate) failed: Vec<String>,
}

/// The phase in progress and the phases that have finished, in run order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProgressView {
    pub(crate) current: Option<PhasePlan>,
    pub(crate) finished: Vec<FinishedPhase>,
}

/// The phases of a run as they start and finish, written by the scenario
/// executor (or by the caller of a single run) and read by the dashboard.
/// Written once per phase start and end, never on the measurement path.
#[derive(Debug, Default)]
pub(crate) struct RunProgress {
    state: Mutex<ProgressView>,
}

impl RunProgress {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProgressView> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that the phase `plan` describes is starting.
    pub(crate) fn begin_phase(&self, plan: PhasePlan) {
        self.state().current = Some(plan);
    }

    /// Records the SLO result of the phase `name`, which has just finished.
    #[cfg_attr(not(feature = "scenario"), allow(dead_code))]
    pub(crate) fn end_phase(&self, name: &str, slo: &SloEvaluation) {
        self.state().finished.push(FinishedPhase {
            name: name.to_string(),
            passed: slo.passed,
            checks: slo.checks.len(),
            failed: slo
                .checks
                .iter()
                .filter(|check| !check.passed)
                .map(|check| check.name.clone())
                .collect(),
        });
    }

    /// A copy of the current state.
    pub(crate) fn view(&self) -> ProgressView {
        self.state().clone()
    }
}

/// The [`DashboardSource`] of a run in this process.
#[derive(Debug, Clone)]
pub(crate) struct RunFeed {
    live: Arc<LiveMetrics>,
    intervals: Arc<LiveIntervals>,
    progress: Arc<RunProgress>,
}

impl RunFeed {
    pub(crate) fn new(
        live: Arc<LiveMetrics>,
        intervals: Arc<LiveIntervals>,
        progress: Arc<RunProgress>,
    ) -> Self {
        Self {
            live,
            intervals,
            progress,
        }
    }
}

impl DashboardSource for RunFeed {
    fn counters(&self) -> LiveSnapshot {
        self.live.snapshot()
    }

    fn run_elapsed(&self) -> Duration {
        self.intervals.elapsed()
    }

    fn phase_clock(&self) -> Option<PhaseClock> {
        self.intervals.phase_clock()
    }

    fn last_interval(&self) -> Option<CompletedInterval> {
        self.intervals.last_completed()
    }

    fn progress(&self) -> ProgressView {
        self.progress.view()
    }
}
