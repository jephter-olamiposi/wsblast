//! Turns what a [`DashboardSource`] reports into the values one frame of the
//! dashboard shows, and keeps the short histories behind its sparklines.

use super::source::{DashboardSource, FinishedPhase, PhasePlan};
use crate::config::{SloThresholds, Stage};
use crate::metrics::LiveSnapshot;
use std::collections::VecDeque;
use std::time::Duration;

/// Points kept for each sparkline; wider than any terminal shows.
const HISTORY: usize = 240;

/// How often a throughput point is added to its sparkline.
const THROUGHPUT_TICK: Duration = Duration::from_millis(500);

/// What the header shows about the run; fixed for the whole run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunInfo {
    pub(crate) target: String,
    pub(crate) mode: String,
}

/// The values one frame of the dashboard shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardView {
    pub(crate) info: RunInfo,
    pub(crate) run_elapsed: Duration,
    pub(crate) counters: LiveSnapshot,
    /// The phase in progress; `None` before the run starts.
    pub(crate) phase: Option<PhaseView>,
    pub(crate) latency: LatencyView,
    pub(crate) window: WindowView,
    /// Connections the run should have open now; `None` before it starts.
    pub(crate) target_connections: Option<usize>,
    /// The current stage of a staged run or phase.
    pub(crate) stage: Option<StageView>,
    /// Messages per second, one point per half second, oldest first.
    pub(crate) throughput_history: Vec<u64>,
    /// p99 of each completed interval in microseconds, oldest first.
    pub(crate) p99_history: Vec<u64>,
    /// Which thresholds [`slo`](Self::slo) shows.
    pub(crate) slo_scope: SloScope,
    pub(crate) slo: Vec<SloRow>,
    /// Finished scenario phases, in run order.
    pub(crate) finished: Vec<FinishedPhase>,
}

/// The phase in progress.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseView {
    /// `None` for a single run.
    pub(crate) name: Option<String>,
    /// 1-based.
    pub(crate) number: usize,
    pub(crate) total: usize,
    /// Time since the phase's clock started; zero while it is starting.
    pub(crate) elapsed: Duration,
    pub(crate) duration: Duration,
    /// The phase was announced but its clock has not started yet (its
    /// target is being resolved), or the previous phase's clock is the one
    /// still registered.
    pub(crate) starting: bool,
}

impl PhaseView {
    /// Fraction of the phase done, between 0 and 1.
    pub(crate) fn ratio(&self) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (self.elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }
}

/// The stage of a staged run that is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StageView {
    /// 1-based.
    pub(crate) number: usize,
    pub(crate) total: usize,
    /// Connections at the end of the stage.
    pub(crate) end_target: usize,
    /// Time left in the stage.
    pub(crate) left: Duration,
}

/// Latency of the most recently completed interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LatencyView {
    /// `rtt` or `response_time`; `None` before the run's clock starts.
    pub(crate) basis: Option<&'static str>,
    /// Samples in the interval; `None` while no interval has completed.
    pub(crate) samples: Option<u64>,
    pub(crate) p50_us: u64,
    pub(crate) p90_us: u64,
    pub(crate) p99_us: u64,
    /// Length of one interval.
    pub(crate) interval: Duration,
}

impl LatencyView {
    /// Whether the percentiles hold values.
    pub(crate) fn has_samples(&self) -> bool {
        self.samples.is_some_and(|count| count > 0)
    }

    /// `response time` or `RTT`, for labels.
    pub(crate) fn basis_label(&self) -> &'static str {
        basis_label(self.basis)
    }
}

/// `response time` for the `response_time` basis, else `RTT`.
pub(crate) fn basis_label(basis: Option<&str>) -> &'static str {
    match basis {
        Some("response_time") => "response time",
        Some(_) => "RTT",
        None => "latency",
    }
}

/// Counter rates over the trailing window of one interval.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct WindowView {
    /// Length of the window actually covered.
    pub(crate) secs: f64,
    pub(crate) errors: u64,
    /// `errors / (messages sent + errors)` in the window; `None` when
    /// nothing was attempted.
    pub(crate) error_rate: Option<f64>,
    /// The larger of messages sent and received, per second.
    pub(crate) throughput: f64,
}

/// Which thresholds the SLO panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SloScope {
    /// The flags of a single run.
    Run,
    /// The current scenario phase's own `slo:`.
    Phase,
    /// The scenario's thresholds, for a phase without any of its own.
    Scenario,
}

/// How a live value compares with its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SloState {
    /// Within the threshold.
    Ok,
    /// Past the threshold right now.
    Breaching,
    /// No live value to compare yet, or none that matches the threshold.
    NoData,
}

/// A threshold: the live value must stay at or below a maximum, or at or
/// above a minimum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Limit {
    Max(f64),
    Min(f64),
}

/// Whether `live` breaches `limit`. Equal to the threshold passes, as in the
/// final SLO evaluation.
pub(crate) fn slo_state(limit: Limit, live: Option<f64>) -> SloState {
    let Some(live) = live.filter(|value| value.is_finite()) else {
        return SloState::NoData;
    };
    let breaching = match limit {
        Limit::Max(max) => live > max,
        Limit::Min(min) => live < min,
    };
    if breaching {
        SloState::Breaching
    } else {
        SloState::Ok
    }
}

/// One configured threshold and its live counterpart.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SloRow {
    pub(crate) name: &'static str,
    /// The threshold, formatted (`<= 25.0ms`).
    pub(crate) threshold: String,
    /// The live value, formatted, or why there is none.
    pub(crate) live: String,
    pub(crate) state: SloState,
}

/// The dashboard's memory between refreshes.
#[derive(Debug)]
pub(crate) struct Dashboard {
    info: RunInfo,
    /// Counter snapshots of the last few seconds, by run time.
    samples: VecDeque<(Duration, LiveSnapshot)>,
    throughput: VecDeque<u64>,
    last_throughput: Option<(Duration, u64)>,
    /// p99 per completed interval, keyed by (phase, interval index).
    p99: VecDeque<((u64, u64), u64)>,
}

impl Dashboard {
    pub(crate) fn new(info: RunInfo) -> Self {
        Self {
            info,
            samples: VecDeque::new(),
            throughput: VecDeque::new(),
            last_throughput: None,
            p99: VecDeque::new(),
        }
    }

    /// Reads `source` once and returns the frame to draw.
    pub(crate) fn update(&mut self, source: &dyn DashboardSource) -> DashboardView {
        let counters = source.counters();
        let run_elapsed = source.run_elapsed();
        let clock = source.phase_clock();
        let progress = source.progress();
        let interval = clock.map_or(Duration::from_secs(1), |clock| clock.interval);

        self.record_throughput(run_elapsed, &counters);
        let window = self.window(run_elapsed, &counters, interval);

        // The clock registered may still be the previous phase's while the
        // next one resolves its target.
        let plan = progress.current.as_ref();
        let current_clock =
            clock.filter(|clock| plan.is_none_or(|plan| clock.seq == plan.index as u64));
        let phase = plan.map(|plan| PhaseView {
            name: plan.name.clone(),
            number: plan.index + 1,
            total: plan.total,
            elapsed: current_clock.map_or(Duration::ZERO, |clock| clock.elapsed),
            duration: plan.duration,
            starting: current_clock.is_none(),
        });

        let mut latency = LatencyView {
            basis: current_clock.map(|clock| clock.basis),
            interval,
            ..LatencyView::default()
        };
        if let (Some(clock), Some(completed)) = (current_clock, source.last_interval())
            && completed.phase == clock.seq
        {
            latency.basis = Some(completed.basis);
            latency.samples = Some(completed.latency.count);
            latency.p50_us = completed.latency.p50_us;
            latency.p90_us = completed.latency.p90_us;
            latency.p99_us = completed.latency.p99_us;
            self.record_p99((completed.phase, completed.index), &latency);
        }

        let (target_connections, stage) = match (plan, current_clock) {
            (Some(plan), Some(clock)) if !clock.ended => (
                Some(target_at(plan, clock.elapsed)),
                stage_at(&plan.stages, clock.elapsed),
            ),
            _ => (None, None),
        };

        let (slo_scope, thresholds) = match plan {
            Some(plan) if plan.name.is_none() => (SloScope::Run, &plan.slo),
            Some(plan) if plan.slo != SloThresholds::default() => (SloScope::Phase, &plan.slo),
            Some(plan) => (SloScope::Scenario, &plan.run_slo),
            None => (SloScope::Run, &SloThresholds::default()),
        };
        let slo = slo_rows(thresholds, &latency, &window);

        DashboardView {
            info: self.info.clone(),
            run_elapsed,
            counters,
            phase,
            latency,
            window,
            target_connections,
            stage,
            throughput_history: self.throughput.iter().copied().collect(),
            p99_history: self.p99.iter().map(|(_, p99)| *p99).collect(),
            slo_scope,
            slo,
            finished: progress.finished,
        }
    }

    fn record_throughput(&mut self, now: Duration, counters: &LiveSnapshot) {
        let messages = counters.messages_sent.max(counters.messages_received);
        match self.last_throughput {
            None => self.last_throughput = Some((now, messages)),
            Some((at, before)) if now >= at + THROUGHPUT_TICK => {
                let secs = (now - at).as_secs_f64();
                let rate = messages.saturating_sub(before) as f64 / secs;
                push_bounded(&mut self.throughput, rate.round() as u64);
                self.last_throughput = Some((now, messages));
            }
            Some(_) => {}
        }
    }

    /// Counter rates over the last `interval` of run time.
    fn window(&mut self, now: Duration, counters: &LiveSnapshot, interval: Duration) -> WindowView {
        self.samples.push_back((now, *counters));
        // Keep the newest sample at least one interval old as the window's
        // start, and drop the ones before it.
        while self.samples.len() > 2 && self.samples[1].0 + interval <= now {
            self.samples.pop_front();
        }
        let Some(&(start, first)) = self.samples.front() else {
            return WindowView::default();
        };
        let secs = now.saturating_sub(start).as_secs_f64();
        if secs <= 0.0 {
            return WindowView::default();
        }
        let errors = counters.total_errors.saturating_sub(first.total_errors);
        let sent = counters.messages_sent.saturating_sub(first.messages_sent);
        let received = counters
            .messages_received
            .saturating_sub(first.messages_received);
        let attempted = sent + errors;
        WindowView {
            secs,
            errors,
            error_rate: (attempted > 0).then(|| errors as f64 / attempted as f64),
            throughput: sent.max(received) as f64 / secs,
        }
    }

    fn record_p99(&mut self, key: (u64, u64), latency: &LatencyView) {
        let p99 = if latency.has_samples() {
            latency.p99_us
        } else {
            0
        };
        // The interval's samples keep arriving for a moment after it ends,
        // so the newest point is refreshed until the next interval ends.
        match self.p99.back_mut() {
            Some((last, value)) if *last == key => *value = p99,
            _ => push_bounded_keyed(&mut self.p99, (key, p99)),
        }
    }
}

fn push_bounded(history: &mut VecDeque<u64>, value: u64) {
    if history.len() >= HISTORY {
        history.pop_front();
    }
    history.push_back(value);
}

fn push_bounded_keyed(history: &mut VecDeque<((u64, u64), u64)>, entry: ((u64, u64), u64)) {
    if history.len() >= HISTORY {
        history.pop_front();
    }
    history.push_back(entry);
}

/// Connections the phase should have open `elapsed` after its clock
/// started: the stage target, the ramp's progress, or every connection.
fn target_at(plan: &PhasePlan, elapsed: Duration) -> usize {
    if !plan.stages.is_empty() {
        return Stage::target_at(&plan.stages, elapsed);
    }
    if plan.ramp_rate == 0 {
        return plan.connections;
    }
    // Workers start at 0, 1/rate, 2/rate, ...
    let started = (elapsed.as_secs_f64() * plan.ramp_rate as f64).floor() as usize + 1;
    started.min(plan.connections)
}

/// The stage in progress `elapsed` after the start; the last stage once
/// every stage has ended.
fn stage_at(stages: &[Stage], elapsed: Duration) -> Option<StageView> {
    let mut end = Duration::ZERO;
    for (index, stage) in stages.iter().enumerate() {
        end += stage.duration;
        if elapsed < end || index + 1 == stages.len() {
            return Some(StageView {
                number: index + 1,
                total: stages.len(),
                end_target: stage.target,
                left: end.saturating_sub(elapsed),
            });
        }
    }
    None
}

fn slo_rows(slo: &SloThresholds, latency: &LatencyView, window: &WindowView) -> Vec<SloRow> {
    let mut rows = Vec::new();
    let has_latency = latency.has_samples();
    let response = latency.basis == Some("response_time");
    let latency_rows: [(&'static str, &'static str, Option<Duration>, Option<u64>); 4] = [
        ("p50", "rt p50", slo.max_p50, Some(latency.p50_us)),
        ("p95", "rt p95", slo.max_p95, None),
        ("p99", "rt p99", slo.max_p99, Some(latency.p99_us)),
        ("p99.9", "rt p99.9", slo.max_p999, None),
    ];
    for (rtt_name, rt_name, max, live) in latency_rows {
        let Some(max) = max else {
            continue;
        };
        let (live_text, live_value) = match live {
            None => ("final only".to_string(), None),
            Some(_) if !has_latency => ("no samples".to_string(), None),
            Some(us) => (format_micros(us), Some(us as f64)),
        };
        rows.push(SloRow {
            name: if response { rt_name } else { rtt_name },
            threshold: format!("<= {}", format_micros(max.as_micros() as u64)),
            live: live_text,
            state: slo_state(Limit::Max(max.as_micros() as f64), live_value),
        });
    }
    if let Some(max) = slo.max_error_rate {
        rows.push(SloRow {
            name: "errors",
            threshold: format!("<= {:.2}%", max * 100.0),
            live: window.error_rate.map_or_else(
                || "none tried".to_string(),
                |rate| format!("~{:.2}%", rate * 100.0),
            ),
            state: slo_state(Limit::Max(max), window.error_rate),
        });
    }
    if let Some(min) = slo.min_throughput {
        let live = (window.secs > 0.0).then_some(window.throughput);
        rows.push(SloRow {
            name: "msg/s",
            threshold: format!(">= {}", format_count(min)),
            live: live.map_or_else(|| "starting".to_string(), format_count),
            state: slo_state(Limit::Min(min), live),
        });
    }
    rows
}

/// `850us`, `12.3ms` or `1.25s`.
pub(crate) fn format_micros(us: u64) -> String {
    if us < 1_000 {
        format!("{us}us")
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    }
}

/// `950`, `12.3k` or `4.56M`.
pub(crate) fn format_count(value: f64) -> String {
    if value < 10_000.0 {
        format!("{value:.0}")
    } else if value < 1_000_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else {
        format!("{:.2}M", value / 1_000_000.0)
    }
}

/// `45s`, `2m05s` or `1h02m`.
pub(crate) fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{:.1}s", duration.as_secs_f64())
    } else if secs < 3_600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_past_its_threshold_is_breaching_and_the_threshold_itself_passes() {
        assert_eq!(slo_state(Limit::Max(25.0), Some(24.9)), SloState::Ok);
        assert_eq!(slo_state(Limit::Max(25.0), Some(25.0)), SloState::Ok);
        assert_eq!(slo_state(Limit::Max(25.0), Some(25.1)), SloState::Breaching);
        assert_eq!(slo_state(Limit::Min(100.0), Some(100.0)), SloState::Ok);
        assert_eq!(
            slo_state(Limit::Min(100.0), Some(99.0)),
            SloState::Breaching
        );
        assert_eq!(slo_state(Limit::Min(100.0), Some(1e9)), SloState::Ok);
        // Without a live value nothing is claimed either way.
        assert_eq!(slo_state(Limit::Max(25.0), None), SloState::NoData);
        assert_eq!(
            slo_state(Limit::Max(25.0), Some(f64::NAN)),
            SloState::NoData
        );
    }

    #[test]
    fn slo_rows_compare_matching_live_values_only() {
        let slo = SloThresholds::new()
            .with_max_p95(Duration::from_millis(10))
            .with_max_p99(Duration::from_millis(25))
            .with_max_error_rate(0.01)
            .with_min_throughput(1_000.0);
        let latency = LatencyView {
            basis: Some("response_time"),
            samples: Some(100),
            p99_us: 30_000,
            ..LatencyView::default()
        };
        let window = WindowView {
            secs: 1.0,
            errors: 0,
            error_rate: Some(0.0),
            throughput: 900.0,
        };
        let rows = slo_rows(&slo, &latency, &window);
        let states: Vec<_> = rows.iter().map(|row| (row.name, row.state)).collect();
        assert_eq!(
            states,
            [
                ("rt p95", SloState::NoData),
                ("rt p99", SloState::Breaching),
                ("errors", SloState::Ok),
                ("msg/s", SloState::Breaching),
            ]
        );
        assert_eq!(rows[1].threshold, "<= 25.0ms");
        assert_eq!(rows[1].live, "30.0ms");

        // An interval without replies says so instead of comparing zeros.
        let empty = LatencyView {
            basis: Some("rtt"),
            samples: Some(0),
            ..LatencyView::default()
        };
        let rows = slo_rows(&slo, &empty, &window);
        assert_eq!((rows[1].name, rows[1].state), ("p99", SloState::NoData));
        assert_eq!(rows[1].live, "no samples");
    }

    #[test]
    fn stages_report_the_stage_in_progress_and_the_time_left() {
        let stages = [
            Stage::new(Duration::from_secs(10), 100),
            Stage::new(Duration::from_secs(20), 100),
            Stage::new(Duration::from_secs(10), 0),
        ];
        let stage = stage_at(&stages, Duration::from_secs(12)).unwrap();
        assert_eq!((stage.number, stage.total, stage.end_target), (2, 3, 100));
        assert_eq!(stage.left, Duration::from_secs(18));
        let past = stage_at(&stages, Duration::from_secs(45)).unwrap();
        assert_eq!((past.number, past.left), (3, Duration::ZERO));
        assert_eq!(stage_at(&[], Duration::ZERO), None);
    }

    #[test]
    fn the_ramp_target_counts_the_workers_started_so_far() {
        let plan = PhasePlan {
            connections: 10,
            ramp_rate: 2,
            ..PhasePlan::default()
        };
        assert_eq!(target_at(&plan, Duration::ZERO), 1);
        assert_eq!(target_at(&plan, Duration::from_millis(1_500)), 4);
        assert_eq!(target_at(&plan, Duration::from_secs(60)), 10);
    }
}
