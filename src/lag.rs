//! Load-generator saturation probe.
//!
//! A task on the run's runtime sleeps [`PROBE_PERIOD`] at a time and
//! measures how late it wakes. Workers wait for the same scheduler, so when
//! the generator is short of CPU, or a task blocks a runtime thread, every
//! timestamp the workers take is late by about as much, and RTT and
//! response time include that delay. See METHODOLOGY.md, section 14.

use crate::metrics::GeneratorLag;
use crate::timeseries::{IntervalLag, TimeSeries};
use hdrhistogram::Histogram;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long the probe sleeps between two measurements.
pub(crate) const PROBE_PERIOD: Duration = Duration::from_millis(10);

/// The p99 lag above which a run's latency is flagged as unreliable.
///
/// Tokio timers fire on the millisecond tick after their deadline, so an
/// idle Linux or macOS machine shows about 1 ms of lag. Windows wakes timers
/// at its default timer resolution of about 15.6 ms, so an idle Windows
/// machine routinely shows up to 16 ms; its threshold keeps the same 20 ms
/// of headroom above that.
pub(crate) const DEFAULT_THRESHOLD: Duration = if cfg!(windows) {
    Duration::from_millis(36)
} else {
    Duration::from_millis(20)
};

/// The share of time-series intervals, in percent, that may be over the
/// threshold before the run is flagged even though its overall p99 is
/// below it.
const INTERVAL_SHARE_PERCENT: u64 = 10;

/// Wake-ups later than the threshold that a run needs, besides a p99 above
/// it, to be flagged. In a short run the p99 is one of the last few
/// samples, and a single scheduling hiccup is not saturation.
const MIN_LATE_IN_RUN: u64 = 3;

/// Wake-ups later than the threshold that an interval needs, besides a p99
/// above it, to count as over the threshold.
const MIN_LATE_IN_INTERVAL: usize = 2;

fn threshold_us() -> u64 {
    DEFAULT_THRESHOLD.as_micros() as u64
}

/// An empty histogram of lag samples, in microseconds.
pub(crate) fn lag_histogram() -> Option<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000, 2).ok()
}

/// The probe of one run, from [`LagProbe::spawn`] to [`LagProbe::finish`].
pub(crate) struct LagProbe {
    task: tokio::task::JoinHandle<LagRecord>,
    stop: CancellationToken,
}

impl LagProbe {
    /// Starts probing on the current runtime. Intervals are counted from
    /// `start` in steps of `interval`, as those of the run's time series.
    pub(crate) fn spawn(start: Instant, interval: Duration) -> Self {
        let stop = CancellationToken::new();
        let token = stop.clone();
        let task = tokio::spawn(async move {
            let mut record = LagRecord::new(start, interval);
            let mut deadline = Instant::now() + PROBE_PERIOD;
            loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep_until(deadline.into()) => {}
                }
                let woke = Instant::now();
                record.record(deadline, woke.saturating_duration_since(deadline));
                deadline = woke + PROBE_PERIOD;
            }
            record.close_interval();
            record
        });
        Self { task, stop }
    }

    /// Stops the probe and returns what it measured.
    pub(crate) async fn finish(self) -> LagRecord {
        self.stop.cancel();
        self.task
            .await
            .unwrap_or_else(|_| LagRecord::new(Instant::now(), Duration::from_secs(1)))
    }
}

/// The lag measured during one run: every sample in one histogram, and the
/// p99 and maximum of each time-series interval.
#[derive(Debug, Clone)]
pub(crate) struct LagRecord {
    start: Instant,
    interval: Duration,
    hist: Option<Histogram<u64>>,
    intervals: BTreeMap<u64, IntervalLag>,
    /// Intervals over the threshold: a p99 above it and at least
    /// [`MIN_LATE_IN_INTERVAL`] wake-ups later than it.
    intervals_over: u64,
    /// The samples of the interval in progress, in microseconds.
    current: Vec<u64>,
    current_index: u64,
}

impl LagRecord {
    fn new(start: Instant, interval: Duration) -> Self {
        Self {
            start,
            interval: interval.max(Duration::from_millis(1)),
            hist: lag_histogram(),
            intervals: BTreeMap::new(),
            intervals_over: 0,
            current: Vec::new(),
            current_index: 0,
        }
    }

    /// Records a wake-up `lag` after a sleep that was due at `due`.
    fn record(&mut self, due: Instant, lag: Duration) {
        let micros = lag.as_micros().clamp(1, 60_000_000) as u64;
        if let Some(hist) = self.hist.as_mut() {
            let _ = hist.record(micros);
        }
        let index = (due.saturating_duration_since(self.start).as_nanos()
            / self.interval.as_nanos()) as u64;
        if index != self.current_index {
            self.close_interval();
            self.current_index = index;
        }
        self.current.push(micros);
    }

    /// Summarises the samples of the interval in progress.
    fn close_interval(&mut self) {
        if self.current.is_empty() {
            return;
        }
        self.current.sort_unstable();
        let n = self.current.len();
        let p99 = self.current[(n * 99).div_ceil(100).max(1) - 1];
        let late = self
            .current
            .iter()
            .filter(|&&us| us > threshold_us())
            .count();
        if p99 > threshold_us() && late >= MIN_LATE_IN_INTERVAL {
            self.intervals_over += 1;
        }
        self.intervals.insert(
            self.current_index,
            IntervalLag {
                samples: n as u64,
                p99_us: p99,
                max_us: self.current[n - 1],
            },
        );
        self.current.clear();
    }

    /// Gives every point of `series`, which starts at this probe's start,
    /// the lag of its interval.
    pub(crate) fn attach(&self, series: &mut TimeSeries) {
        let interval = self.interval.as_secs_f64();
        for point in &mut series.points {
            let index = (point.start_secs / interval).round().max(0.0) as u64;
            point.generator_lag = self.intervals.get(&index).cloned();
        }
    }

    /// The totals of this record, to summarise or to add to those of other
    /// scenario phases.
    pub(crate) fn totals(&self) -> LagTotals {
        LagTotals {
            hist: self.hist.clone(),
            intervals: self.intervals.len() as u64,
            intervals_over: self.intervals_over,
            threshold_us: None,
        }
    }
}

/// Lag totals that add up across scenario phases.
#[derive(Debug, Clone, Default)]
pub(crate) struct LagTotals {
    hist: Option<Histogram<u64>>,
    intervals: u64,
    intervals_over: u64,
    /// The threshold of the machine the samples were taken on, when it is
    /// not this one: a distributed agent's, whose platform may differ from
    /// its coordinator's. `None` for this machine's.
    threshold_us: Option<u64>,
}

impl LagTotals {
    /// The totals as a distributed agent sends them: the samples, the
    /// interval counts and the threshold the intervals were checked
    /// against.
    #[cfg(feature = "distributed")]
    pub(crate) fn parts(&self) -> (Option<&Histogram<u64>>, u64, u64, u64) {
        (
            self.hist.as_ref(),
            self.intervals,
            self.intervals_over,
            self.threshold_us.unwrap_or_else(threshold_us),
        )
    }

    /// Totals an agent sent, checked against its own threshold.
    #[cfg(feature = "distributed")]
    pub(crate) fn from_parts(
        hist: Histogram<u64>,
        intervals: u64,
        intervals_over: u64,
        threshold_us: u64,
    ) -> Self {
        Self {
            hist: Some(hist),
            intervals,
            intervals_over,
            threshold_us: Some(threshold_us),
        }
    }

    /// Adds the totals of another phase.
    #[cfg(feature = "scenario")]
    pub(crate) fn add(&mut self, other: &Self) {
        match (self.hist.as_mut(), other.hist.as_ref()) {
            (Some(hist), Some(other)) => {
                let _ = hist.add(other);
            }
            (None, Some(other)) => self.hist = Some(other.clone()),
            (_, None) => {}
        }
        self.intervals += other.intervals;
        self.intervals_over += other.intervals_over;
        self.threshold_us = self.threshold_us.or(other.threshold_us);
    }

    /// The report's summary; `None` without a single sample.
    pub(crate) fn summary(&self) -> Option<GeneratorLag> {
        let hist = self.hist.as_ref().filter(|hist| !hist.is_empty())?;
        let threshold_us = self.threshold_us.unwrap_or_else(threshold_us);
        let p99_us = hist.value_at_quantile(0.99);
        let late = hist.len() - hist.count_between(1, threshold_us);
        let p99_over = p99_us > threshold_us && late >= MIN_LATE_IN_RUN;
        let share_over = self.intervals_over > 0
            && self.intervals_over * 100 >= self.intervals * INTERVAL_SHARE_PERCENT;
        Some(GeneratorLag {
            probe_interval_us: PROBE_PERIOD.as_micros() as u64,
            samples: hist.len(),
            p50_us: hist.value_at_quantile(0.50),
            p99_us,
            max_us: hist.max(),
            threshold_us,
            intervals: self.intervals,
            intervals_over_threshold: self.intervals_over,
            saturated: p99_over || share_over,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record of `(due_ms, lag_ms)` samples, taken in time order as the
    /// probe takes them.
    fn record_of(lags_ms: &[(u64, u64)]) -> LagRecord {
        let start = Instant::now();
        let mut record = LagRecord::new(start, Duration::from_secs(1));
        let mut lags_ms = lags_ms.to_vec();
        lags_ms.sort_by_key(|&(at_ms, _)| at_ms);
        for (at_ms, lag_ms) in lags_ms {
            record.record(
                start + Duration::from_millis(at_ms),
                Duration::from_millis(lag_ms),
            );
        }
        record.close_interval();
        record
    }

    #[test]
    fn intervals_hold_their_own_p99_and_maximum() {
        let mut lags: Vec<(u64, u64)> = (0..100).map(|i| (i * 10, 1)).collect();
        lags.extend((0..100).map(|i| (1_000 + i * 10, if i == 50 { 80 } else { 2 })));
        let record = record_of(&lags);
        assert_eq!(record.intervals.len(), 2);
        let first = &record.intervals[&0];
        assert_eq!(
            (first.samples, first.p99_us, first.max_us),
            (100, 1_000, 1_000)
        );
        let second = &record.intervals[&1];
        assert_eq!(
            (second.samples, second.p99_us, second.max_us),
            (100, 2_000, 80_000)
        );

        let summary = record.totals().summary().expect("samples");
        assert_eq!(summary.samples, 200);
        assert!(!summary.saturated, "{summary:?}");
        assert!((79_000..=81_000).contains(&summary.max_us), "{summary:?}");
    }

    #[test]
    fn a_high_p99_or_enough_bad_intervals_flag_saturation() {
        let all_late: Vec<(u64, u64)> = (0..100).map(|i| (i * 10, 50)).collect();
        assert!(record_of(&all_late).totals().summary().unwrap().saturated);

        // One bad interval out of five: the run's p99 is above the
        // threshold too, since a fifth of the samples are late.
        let mut lags: Vec<(u64, u64)> = (0..400).map(|i| (i * 10, 1)).collect();
        lags.extend((0..100).map(|i| (4_000 + i * 10, 50)));
        let summary = record_of(&lags).totals().summary().unwrap();
        assert_eq!(summary.intervals_over_threshold, 1);
        assert!(summary.saturated);

        // A single late wake-up in a short run is a hiccup, not saturation.
        let mut lags: Vec<(u64, u64)> = (0..50).map(|i| (i * 10, 1)).collect();
        lags.push((505, 80));
        let summary = record_of(&lags).totals().summary().unwrap();
        assert!(summary.p99_us > summary.threshold_us);
        assert_eq!(summary.intervals_over_threshold, 0);
        assert!(!summary.saturated, "{summary:?}");

        // A short burst inside one interval of twenty stays below both.
        let mut lags: Vec<(u64, u64)> = (0..2_000).map(|i| (i * 10, 1)).collect();
        lags.extend((0..5).map(|i| (500 + i, 50)));
        let summary = record_of(&lags).totals().summary().unwrap();
        assert!(!summary.saturated, "{summary:?}");
    }

    #[test]
    fn no_samples_means_no_summary() {
        assert!(record_of(&[]).totals().summary().is_none());
        assert!(LagTotals::default().summary().is_none());
    }

    #[test]
    fn an_idle_runtime_shows_little_lag() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let summary = runtime.block_on(async {
            let probe = LagProbe::spawn(Instant::now(), Duration::from_secs(1));
            tokio::time::sleep(Duration::from_millis(400)).await;
            probe.finish().await.totals().summary().expect("samples")
        });
        // Other processes may delay a wake-up now and then; the median
        // stays at the timer's granularity.
        assert!(summary.samples >= 10, "{summary:?}");
        assert!(summary.p50_us < 20_000, "{summary:?}");
    }

    #[test]
    fn a_blocked_runtime_is_flagged() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let summary = runtime.block_on(async {
            let probe = LagProbe::spawn(Instant::now(), Duration::from_secs(1));
            // A task that blocks the only runtime thread, as CPU-bound work
            // or a blocking call in a task would.
            let blocker = tokio::spawn(async {
                for _ in 0..8 {
                    std::thread::sleep(Duration::from_millis(60));
                    tokio::task::yield_now().await;
                }
            });
            blocker.await.unwrap();
            probe.finish().await.totals().summary().expect("samples")
        });
        assert!(summary.saturated, "{summary:?}");
        assert!(summary.max_us >= 40_000, "{summary:?}");
    }
}
