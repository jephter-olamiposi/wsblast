//! Per-interval metrics over the course of a run.
//!
//! Counters come from the run's [`LiveMetrics`], sampled by one task at each
//! interval boundary, so workers do no extra work for them. Latency comes from
//! a small histogram each worker keeps for the current interval and hands to
//! the shared [`IntervalStore`] when its interval changes, so the store's lock
//! is taken about once per interval per worker, never per message.

use crate::metrics::{LiveMetrics, LiveSnapshot};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The most points a series holds. Longer runs use longer intervals.
const MAX_POINTS: u64 = 600;

/// Per-interval histograms use 2 significant figures (1% precision) to keep
/// one per worker at about 20 KB.
const INTERVAL_SIGFIG: u8 = 2;

/// The interval length for a run of `duration`: 1 second for runs up to
/// [`MAX_POINTS`] seconds, otherwise the whole number of seconds that keeps
/// the series within [`MAX_POINTS`] points.
pub(crate) fn interval_for(duration: Duration) -> Duration {
    let secs = duration.as_secs().div_ceil(MAX_POINTS).max(1);
    Duration::from_secs(secs)
}

pub(crate) fn interval_histogram() -> Option<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000, INTERVAL_SIGFIG).ok()
}

/// Latency histograms of a run, one per interval, merged from every worker.
#[derive(Debug)]
pub(crate) struct IntervalStore {
    start: Instant,
    interval: Duration,
    buckets: Mutex<BTreeMap<u64, Histogram<u64>>>,
}

impl IntervalStore {
    pub(crate) fn new(start: Instant, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            start,
            interval,
            buckets: Mutex::new(BTreeMap::new()),
        })
    }

    fn bucket_of(&self, at: Instant) -> u64 {
        (at.saturating_duration_since(self.start).as_nanos() / self.interval.as_nanos()) as u64
    }

    /// When the run's clock started.
    pub(crate) fn start(&self) -> Instant {
        self.start
    }

    /// The length of one interval.
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// A copy of interval `bucket`'s histogram, if it has samples, for a
    /// distributed agent's live snapshot.
    #[cfg(feature = "distributed")]
    pub(crate) fn bucket(&self, bucket: u64) -> Option<Histogram<u64>> {
        self.buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&bucket)
            .cloned()
    }

    /// Latency of interval `bucket` from the samples workers have handed
    /// over so far. The lock is held only while the percentiles are read.
    pub(crate) fn latency_of(&self, bucket: u64) -> IntervalLatency {
        let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        IntervalLatency::from_histogram(buckets.get(&bucket))
    }

    /// A copy of every interval's histogram, keyed by interval index, for a
    /// distributed agent to send to its coordinator.
    #[cfg(feature = "distributed")]
    pub(crate) fn buckets(&self) -> BTreeMap<u64, Histogram<u64>> {
        self.buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn absorb(&self, bucket: u64, hist: &Histogram<u64>) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        match buckets.get_mut(&bucket) {
            Some(existing) => {
                let _ = existing.add(hist);
            }
            None => {
                buckets.insert(bucket, hist.clone());
            }
        }
    }
}

/// The time series of the run or scenario phase in progress, as the live
/// readers (the exporters and the TUI) see it while the run lasts.
///
/// The runner registers each run or phase's [`IntervalStore`] when its clock
/// starts and marks it ended when its workers have stopped. Readers take the
/// lock briefly to find the phase, and the store's lock only while one
/// interval's percentiles are read, so nothing here touches the measurement
/// path.
#[derive(Debug, Default)]
pub(crate) struct LiveIntervals {
    state: Mutex<LiveState>,
}

#[derive(Debug, Default)]
struct LiveState {
    run_start: Option<Instant>,
    finished: Option<Duration>,
    phase: Option<LivePhase>,
    /// Phases registered so far.
    phases_begun: u64,
    /// Set while a distributed coordinator runs a phase on its agents: the
    /// latency it merged from their interval histograms replaces the local
    /// time series, which a coordinator does not have.
    #[cfg(feature = "distributed")]
    remote: Option<RemotePhase>,
}

/// A phase that runs on distributed agents, as the coordinator sees it.
#[cfg(feature = "distributed")]
#[derive(Debug)]
struct RemotePhase {
    start: Instant,
    interval: Duration,
    basis: &'static str,
    ended: Option<Duration>,
    seq: u64,
    /// The last interval the coordinator merged: its index and latency.
    latest: Option<(u64, IntervalLatency)>,
}

/// The time series of the run or scenario phase in progress.
#[derive(Debug)]
struct LivePhase {
    store: Arc<IntervalStore>,
    basis: &'static str,
    /// The phase's `elapsed`, once it has ended.
    ended: Option<Duration>,
    /// 0 for the first phase registered, 1 for the next, and so on.
    seq: u64,
}

/// The clock of the run or scenario phase in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhaseClock {
    /// 0 for the first phase of the run, 1 for the next, and so on; a single
    /// run has one phase.
    pub(crate) seq: u64,
    /// Time since the phase's clock started, or its length once it ended.
    pub(crate) elapsed: Duration,
    /// Whether the phase has ended.
    pub(crate) ended: bool,
    /// Length of one time-series interval.
    pub(crate) interval: Duration,
    /// `rtt` or `response_time`, as the time series' `latency_basis`.
    pub(crate) basis: &'static str,
}

/// Latency of the most recently completed interval of the phase in progress.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompletedInterval {
    /// The [`PhaseClock::seq`] of the phase the interval belongs to.
    pub(crate) phase: u64,
    /// Index of the interval within its phase.
    pub(crate) index: u64,
    /// `rtt` or `response_time`, as the time series' `latency_basis`.
    pub(crate) basis: &'static str,
    /// The interval's percentiles from the samples handed over so far.
    pub(crate) latency: IntervalLatency,
}

impl LiveIntervals {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LiveState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers the time series of a run or phase whose clock just started.
    /// The first call also starts the run's clock.
    pub(crate) fn begin_phase(&self, store: Arc<IntervalStore>, basis: &'static str) {
        let mut state = self.state();
        state.run_start.get_or_insert(store.start());
        let seq = state.phases_begun;
        state.phases_begun += 1;
        #[cfg(feature = "distributed")]
        {
            state.remote = None;
        }
        state.phase = Some(LivePhase {
            store,
            basis,
            ended: None,
            seq,
        });
    }

    /// Marks the current phase as ended after `elapsed`, once every worker
    /// has handed over its last interval.
    pub(crate) fn end_phase(&self, elapsed: Duration) {
        let mut state = self.state();
        #[cfg(feature = "distributed")]
        if let Some(remote) = state.remote.as_mut() {
            remote.ended = Some(elapsed);
        }
        if let Some(phase) = state.phase.as_mut() {
            phase.ended = Some(elapsed);
        }
    }

    /// Registers a phase that a distributed coordinator runs on its agents,
    /// whose clock started at `start`. Until [`LiveIntervals::publish_remote`]
    /// is called, the phase has no completed interval.
    #[cfg(feature = "distributed")]
    pub(crate) fn begin_remote_phase(
        &self,
        start: Instant,
        interval: Duration,
        basis: &'static str,
    ) {
        let mut state = self.state();
        state.run_start.get_or_insert(start);
        let seq = state.phases_begun;
        state.phases_begun += 1;
        state.phase = None;
        state.remote = Some(RemotePhase {
            start,
            interval,
            basis,
            ended: None,
            seq,
            latest: None,
        });
    }

    /// Publishes the latency of interval `index` of the remote phase, merged
    /// from every agent's histogram for that interval.
    #[cfg(feature = "distributed")]
    pub(crate) fn publish_remote(&self, index: u64, latency: IntervalLatency) {
        if let Some(remote) = self.state().remote.as_mut() {
            remote.latest = Some((index, latency));
        }
    }

    /// Freezes the run's elapsed time once the whole run has ended.
    pub(crate) fn finish(&self) {
        let mut state = self.state();
        if state.finished.is_none() {
            state.finished = Some(state.run_start.map_or(Duration::ZERO, |s| s.elapsed()));
        }
    }

    /// Time since the run's clock started, frozen by
    /// [`LiveIntervals::finish`]; zero before the run starts.
    pub(crate) fn elapsed(&self) -> Duration {
        let state = self.state();
        state
            .finished
            .or_else(|| state.run_start.map(|start| start.elapsed()))
            .unwrap_or_default()
    }

    /// The clock of the current phase; `None` before the first one starts.
    pub(crate) fn phase_clock(&self) -> Option<PhaseClock> {
        let state = self.state();
        #[cfg(feature = "distributed")]
        if let Some(remote) = state.remote.as_ref() {
            return Some(PhaseClock {
                seq: remote.seq,
                elapsed: remote.ended.unwrap_or_else(|| remote.start.elapsed()),
                ended: remote.ended.is_some(),
                interval: remote.interval,
                basis: remote.basis,
            });
        }
        let phase = state.phase.as_ref()?;
        Some(PhaseClock {
            seq: phase.seq,
            elapsed: phase.ended.unwrap_or_else(|| phase.store.start().elapsed()),
            ended: phase.ended.is_some(),
            interval: phase.store.interval(),
            basis: phase.basis,
        })
    }

    /// Latency of the last interval of the current phase that has ended:
    /// `None` before the phase's first interval ends. Once the phase has
    /// ended, the last interval that ended before it did stays current.
    pub(crate) fn last_completed(&self) -> Option<CompletedInterval> {
        let state = self.state();
        #[cfg(feature = "distributed")]
        if let Some(remote) = state.remote.as_ref() {
            let (index, latency) = remote.latest.clone()?;
            return Some(CompletedInterval {
                phase: remote.seq,
                index,
                basis: remote.basis,
                latency,
            });
        }
        let phase = state.phase.as_ref()?;
        let since_start = phase.ended.unwrap_or_else(|| phase.store.start().elapsed());
        let index = last_completed(since_start, phase.store.interval())?;
        Some(CompletedInterval {
            phase: phase.seq,
            index,
            basis: phase.basis,
            latency: phase.store.latency_of(index),
        })
    }
}

/// Index of the last interval that ended at or before `since_start`.
pub(crate) fn last_completed(since_start: Duration, interval: Duration) -> Option<u64> {
    let index = since_start.as_nanos() / interval.as_nanos().max(1);
    u64::try_from(index).ok()?.checked_sub(1)
}

/// A worker's latency samples for the interval it is currently in.
#[derive(Clone, Debug)]
pub(crate) struct IntervalRecorder {
    store: Arc<IntervalStore>,
    bucket: u64,
    hist: Histogram<u64>,
}

impl IntervalRecorder {
    pub(crate) fn new(store: Arc<IntervalStore>) -> Option<Self> {
        Some(Self {
            store,
            bucket: 0,
            hist: interval_histogram()?,
        })
    }

    #[inline]
    pub(crate) fn record(&mut self, micros: u64) {
        let bucket = self.store.bucket_of(Instant::now());
        if bucket != self.bucket {
            self.flush();
            self.bucket = bucket;
        }
        let _ = self.hist.record(micros);
    }

    /// Hands the samples held for the current interval to the store.
    pub(crate) fn flush(&mut self) {
        if !self.hist.is_empty() {
            self.store.absorb(self.bucket, &self.hist);
            self.hist.reset();
        }
    }
}

/// Counter snapshots taken at each interval boundary of a run.
pub(crate) struct CounterSampler {
    task: tokio::task::JoinHandle<Vec<(Duration, LiveSnapshot)>>,
    live: Arc<LiveMetrics>,
}

impl CounterSampler {
    /// Starts sampling `live` every `interval` from `start` until `cancel`
    /// fires. The last sample is added by [`CounterSampler::finish`].
    pub(crate) fn spawn(
        live: Arc<LiveMetrics>,
        start: Instant,
        interval: Duration,
        cancel: CancellationToken,
    ) -> Self {
        let task_live = Arc::clone(&live);
        let task = tokio::spawn(async move {
            let live = task_live;
            // The registry may already hold counts from earlier scenario
            // phases, so the first sample is the baseline for the deltas.
            let mut samples = vec![(Duration::ZERO, live.snapshot())];
            let mut ticker = tokio::time::interval_at((start + interval).into(), interval);
            // A late tick is taken at once and the next one stays on its boundary,
            // so every sample lines up with the latency buckets.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        samples.push((start.elapsed(), live.snapshot()));
                    }
                }
            }
            samples
        });
        Self { task, live }
    }

    /// Returns the samples once every worker has stopped. The last sample is
    /// taken now, so a message a worker finished sending while the run was
    /// being cancelled is counted, and is stamped `end`, the moment the run
    /// was cancelled, so the last interval ends where the run did.
    pub(crate) async fn finish(self, end: Duration) -> Vec<(Duration, LiveSnapshot)> {
        let mut samples = self.task.await.unwrap_or_default();
        let end = samples.last().map_or(end, |(last, _)| end.max(*last));
        samples.push((end, self.live.snapshot()));
        samples
    }
}

/// Metrics of a run broken down by time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TimeSeries {
    /// Length of each interval in seconds. The last point of a run, or of a
    /// scenario phase, may be shorter.
    pub interval_secs: f64,
    /// The latency each point reports: `response_time` for paced echo runs
    /// and paced echo action scripts, `rtt` otherwise.
    pub latency_basis: String,
    /// One point per interval, in time order.
    pub points: Vec<TimeSeriesPoint>,
}

/// Metrics of one interval of a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TimeSeriesPoint {
    /// Start of the interval, in seconds since the run started.
    pub start_secs: f64,
    /// Length of the interval in seconds.
    pub duration_secs: f64,
    /// Connections open at the end of the interval.
    pub active_connections: u64,
    /// Data frames sent during the interval.
    pub messages_sent: u64,
    /// Data frames received during the interval.
    pub messages_recv: u64,
    /// Payload bytes sent during the interval.
    pub bytes_sent: u64,
    /// Payload bytes received during the interval.
    pub bytes_recv: u64,
    /// Errors recorded during the interval.
    pub errors: u64,
    /// The larger of messages sent and received, per second of the interval.
    pub throughput_msg_per_sec: f64,
    /// Latency of the replies received during the interval; empty when none
    /// were.
    pub latency: IntervalLatency,
    /// How late the generator's runtime woke up during the interval; absent
    /// when the probe took no sample in it, and from distributed runs and
    /// reports written before it was added. See METHODOLOGY.md, section 14.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator_lag: Option<IntervalLag>,
}

/// The load generator's wake-up lag during one interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IntervalLag {
    /// Number of probe wake-ups in the interval.
    pub samples: u64,
    /// 99th percentile lag, exact to the microsecond.
    pub p99_us: u64,
    /// Largest lag.
    pub max_us: u64,
}

/// Latency percentiles of one interval, at 1% precision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IntervalLatency {
    /// Number of samples.
    pub count: u64,
    /// Median.
    pub p50_us: u64,
    /// 90th percentile.
    pub p90_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// Largest sample.
    pub max_us: u64,
}

impl IntervalLatency {
    pub(crate) fn from_histogram(hist: Option<&Histogram<u64>>) -> Self {
        match hist {
            Some(hist) if !hist.is_empty() => Self {
                count: hist.len(),
                p50_us: hist.value_at_quantile(0.50),
                p90_us: hist.value_at_quantile(0.90),
                p99_us: hist.value_at_quantile(0.99),
                max_us: hist.max(),
            },
            _ => Self::default(),
        }
    }
}

impl TimeSeries {
    /// Builds the series from the counter samples and latency histograms of
    /// one run. The first sample is the baseline taken when the run started.
    pub(crate) fn build(
        samples: &[(Duration, LiveSnapshot)],
        store: &IntervalStore,
        latency_basis: &str,
    ) -> Self {
        let buckets = store.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut points = Vec::with_capacity(samples.len());
        let Some((&first, rest)) = samples.split_first() else {
            return Self::default();
        };
        let mut previous = first;
        let interval_nanos = store.interval.as_nanos().max(1);
        for (at, snap) in rest {
            let (prev_at, prev) = previous;
            let duration = at.saturating_sub(prev_at);
            // A sample taken at the very end of the run, right on a boundary,
            // adds an empty interval.
            if duration.is_zero() {
                continue;
            }
            let messages_sent = snap.messages_sent.saturating_sub(prev.messages_sent);
            let messages_recv = snap
                .messages_received
                .saturating_sub(prev.messages_received);
            let secs = duration.as_secs_f64().max(0.000_1);
            points.push(TimeSeriesPoint {
                start_secs: prev_at.as_secs_f64(),
                duration_secs: duration.as_secs_f64(),
                active_connections: snap.active_connections,
                messages_sent,
                messages_recv,
                bytes_sent: snap.bytes_sent.saturating_sub(prev.bytes_sent),
                bytes_recv: snap.bytes_received.saturating_sub(prev.bytes_received),
                errors: snap.total_errors.saturating_sub(prev.total_errors),
                throughput_msg_per_sec: messages_sent.max(messages_recv) as f64 / secs,
                latency: IntervalLatency::from_histogram(
                    buckets.get(&((prev_at.as_nanos() / interval_nanos) as u64)),
                ),
                generator_lag: None,
            });
            previous = (*at, *snap);
        }
        // The last sample is taken when the run ends, which can be a moment
        // after a boundary. A sliver that short has no meaningful rate, so it
        // is folded into the interval before it.
        if points.len() >= 2
            && points[points.len() - 1].duration_secs < store.interval.as_secs_f64() / 20.0
        {
            let tail = points.pop().expect("checked length");
            let last = points.last_mut().expect("checked length");
            last.duration_secs += tail.duration_secs;
            last.active_connections = tail.active_connections;
            last.messages_sent += tail.messages_sent;
            last.messages_recv += tail.messages_recv;
            last.bytes_sent += tail.bytes_sent;
            last.bytes_recv += tail.bytes_recv;
            last.errors += tail.errors;
            last.throughput_msg_per_sec =
                last.messages_sent.max(last.messages_recv) as f64 / last.duration_secs.max(0.000_1);
        }
        Self {
            interval_secs: store.interval.as_secs_f64(),
            latency_basis: latency_basis.to_string(),
            points,
        }
    }

    /// Appends `next`, a later run such as the next scenario phase, shifting
    /// its points to start `offset` after this series' start.
    #[cfg(feature = "scenario")]
    pub(crate) fn append(&mut self, next: TimeSeries, offset: Duration) {
        if self.points.is_empty() {
            self.interval_secs = next.interval_secs;
            self.latency_basis = next.latency_basis.clone();
        } else if self.latency_basis != next.latency_basis {
            self.latency_basis = "mixed".to_string();
        }
        let offset = offset.as_secs_f64();
        self.points.extend(next.points.into_iter().map(|mut point| {
            point.start_secs += offset;
            point
        }));
    }
}

/// One machine's part of a distributed run: its series and the per-interval
/// latency histograms the series was built from, keyed by interval index.
#[cfg(feature = "distributed")]
pub(crate) struct SeriesPart {
    pub(crate) series: TimeSeries,
    pub(crate) buckets: BTreeMap<u64, Histogram<u64>>,
}

#[cfg(feature = "distributed")]
impl TimeSeries {
    /// Merges the series of machines that ran at the same time, each counted
    /// from its own start. Points with the same interval index are added up:
    /// counters and open connections are summed, the duration is the longest
    /// of them, and the latency is read from the sum of the interval
    /// histograms, so interval percentiles are those of every sample, not an
    /// average of percentiles.
    pub(crate) fn merge_parallel(parts: Vec<SeriesPart>) -> Self {
        let Some(first) = parts.iter().find(|part| !part.series.points.is_empty()) else {
            return parts
                .into_iter()
                .next()
                .map(|part| part.series)
                .unwrap_or_default();
        };
        let interval_secs = first.series.interval_secs;
        let latency_basis = first.series.latency_basis.clone();
        let index_of = |start_secs: f64| {
            if interval_secs > 0.0 {
                (start_secs / interval_secs).round().max(0.0) as u64
            } else {
                0
            }
        };

        let mut points: BTreeMap<u64, TimeSeriesPoint> = BTreeMap::new();
        let mut histograms: BTreeMap<u64, Histogram<u64>> = BTreeMap::new();
        for part in parts {
            for point in part.series.points {
                let merged =
                    points
                        .entry(index_of(point.start_secs))
                        .or_insert_with(|| TimeSeriesPoint {
                            start_secs: point.start_secs,
                            ..TimeSeriesPoint::default()
                        });
                merged.start_secs = merged.start_secs.min(point.start_secs);
                merged.duration_secs = merged.duration_secs.max(point.duration_secs);
                merged.active_connections += point.active_connections;
                merged.messages_sent += point.messages_sent;
                merged.messages_recv += point.messages_recv;
                merged.bytes_sent += point.bytes_sent;
                merged.bytes_recv += point.bytes_recv;
                merged.errors += point.errors;
            }
            for (bucket, hist) in part.buckets {
                match histograms.get_mut(&bucket) {
                    Some(existing) => {
                        let _ = existing.add(&hist);
                    }
                    None => {
                        histograms.insert(bucket, hist);
                    }
                }
            }
        }

        let points = points
            .into_iter()
            .map(|(index, mut point)| {
                point.throughput_msg_per_sec = point.messages_sent.max(point.messages_recv) as f64
                    / point.duration_secs.max(0.000_1);
                point.latency = IntervalLatency::from_histogram(histograms.get(&index));
                point
            })
            .collect();
        Self {
            interval_secs,
            latency_basis,
            points,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_completed_interval_is_the_one_before_the_current_one() {
        let second = Duration::from_secs(1);
        assert_eq!(last_completed(Duration::from_millis(999), second), None);
        assert_eq!(last_completed(Duration::from_secs(1), second), Some(0));
        assert_eq!(
            last_completed(Duration::from_millis(2_500), second),
            Some(1)
        );
        assert_eq!(last_completed(Duration::ZERO, Duration::ZERO), None);
    }

    #[test]
    fn live_intervals_number_phases_and_read_the_completed_interval() {
        let live = LiveIntervals::new();
        assert_eq!(live.phase_clock(), None);
        assert_eq!(live.last_completed(), None);
        assert_eq!(live.elapsed(), Duration::ZERO);

        let start = Instant::now()
            .checked_sub(Duration::from_millis(1_500))
            .unwrap();
        let store = IntervalStore::new(start, Duration::from_secs(1));
        let mut hist = interval_histogram().unwrap();
        hist.record(2_000).unwrap();
        store.absorb(0, &hist);
        live.begin_phase(Arc::clone(&store), "rtt");
        let clock = live.phase_clock().unwrap();
        assert_eq!((clock.seq, clock.ended, clock.basis), (0, false, "rtt"));
        assert!(clock.elapsed >= Duration::from_millis(1_500));
        let completed = live.last_completed().unwrap();
        assert_eq!((completed.phase, completed.index), (0, 0));
        assert_eq!(completed.latency.count, 1);

        live.end_phase(Duration::from_millis(1_600));
        assert!(live.phase_clock().unwrap().ended);

        // The next phase starts again at interval 0 with its own number.
        live.begin_phase(IntervalStore::new(Instant::now(), store.interval()), "rtt");
        assert_eq!(live.phase_clock().unwrap().seq, 1);
        assert_eq!(live.last_completed(), None);
    }

    #[test]
    fn interval_keeps_series_within_the_point_limit() {
        assert_eq!(interval_for(Duration::from_secs(1)), Duration::from_secs(1));
        assert_eq!(
            interval_for(Duration::from_secs(600)),
            Duration::from_secs(1)
        );
        assert_eq!(
            interval_for(Duration::from_secs(601)),
            Duration::from_secs(2)
        );
        assert_eq!(
            interval_for(Duration::from_secs(3600)),
            Duration::from_secs(6)
        );
        assert_eq!(
            interval_for(Duration::from_secs(86_400)),
            Duration::from_secs(144)
        );
    }

    #[test]
    fn points_hold_counter_deltas_and_their_interval_latency() {
        let start = Instant::now();
        let store = IntervalStore::new(start, Duration::from_secs(1));
        let mut hist = interval_histogram().unwrap();
        hist.record(2_000).unwrap();
        store.absorb(1, &hist);
        store.absorb(1, &hist);

        let snap = |sent, recv, errors| LiveSnapshot {
            messages_sent: sent,
            messages_received: recv,
            total_errors: errors,
            ..LiveSnapshot::default()
        };
        let samples = [
            (Duration::ZERO, snap(0, 0, 0)),
            (Duration::from_secs(1), snap(10, 10, 0)),
            (Duration::from_secs(2), snap(30, 28, 1)),
            (Duration::from_millis(2_500), snap(35, 35, 1)),
        ];
        let series = TimeSeries::build(&samples, &store, "rtt");

        assert_eq!(series.points.len(), 3);
        let second = &series.points[1];
        assert_eq!((second.start_secs, second.duration_secs), (1.0, 1.0));
        assert_eq!((second.messages_sent, second.messages_recv), (20, 18));
        assert_eq!(second.errors, 1);
        assert_eq!(second.throughput_msg_per_sec, 20.0);
        assert_eq!(second.latency.count, 2);
        assert!((1_980..=2_020).contains(&second.latency.p99_us));
        assert_eq!(series.points[0].latency.count, 0);

        let mut with_sliver = samples.to_vec();
        with_sliver.push((Duration::from_millis(2_510), snap(36, 36, 1)));
        let sliver = TimeSeries::build(&with_sliver, &store, "rtt");
        assert_eq!(sliver.points.len(), 3);
        let folded = &sliver.points[2];
        assert!((folded.duration_secs - 0.51).abs() < 1e-9);
        assert_eq!(folded.messages_sent, 6);
        let last = &series.points[2];
        assert_eq!(last.duration_secs, 0.5);
        assert_eq!(last.throughput_msg_per_sec, 14.0);
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn parallel_series_add_counters_and_merge_interval_histograms() {
        let point = |start: f64, duration: f64, sent: u64, active: u64| TimeSeriesPoint {
            start_secs: start,
            duration_secs: duration,
            active_connections: active,
            messages_sent: sent,
            messages_recv: sent,
            ..TimeSeriesPoint::default()
        };
        let series = |points| TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        };
        let hist_of = |values: &[u64]| {
            let mut hist = interval_histogram().unwrap();
            for value in values {
                hist.record(*value).unwrap();
            }
            hist
        };
        let a = SeriesPart {
            series: series(vec![point(0.0, 1.0, 10, 2), point(1.0, 0.5, 4, 2)]),
            buckets: BTreeMap::from([(0, hist_of(&[1_000; 9])), (1, hist_of(&[2_000]))]),
        };
        let b = SeriesPart {
            series: series(vec![point(0.0, 1.0, 6, 3), point(1.0, 0.52, 6, 1)]),
            buckets: BTreeMap::from([(0, hist_of(&[50_000]))]),
        };
        let merged = TimeSeries::merge_parallel(vec![a, b]);
        assert_eq!(merged.points.len(), 2);
        let first = &merged.points[0];
        assert_eq!((first.messages_sent, first.active_connections), (16, 5));
        assert_eq!(first.throughput_msg_per_sec, 16.0);
        // Ten samples: nine at 1 ms and one at 50 ms, so p50 is 1 ms and
        // p99 is 50 ms, which no average of the two parts would give.
        assert_eq!(first.latency.count, 10);
        assert!((990..=1_010).contains(&first.latency.p50_us));
        assert!((49_500..=50_500).contains(&first.latency.p99_us));
        let second = &merged.points[1];
        assert_eq!(second.duration_secs, 0.52);
        assert_eq!(second.messages_sent, 10);
        assert_eq!(second.latency.count, 1);
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn appended_phases_are_shifted_and_mixed_bases_are_labelled() {
        let point = |start| TimeSeriesPoint {
            start_secs: start,
            duration_secs: 1.0,
            ..TimeSeriesPoint::default()
        };
        let mut series = TimeSeries::default();
        series.append(
            TimeSeries {
                interval_secs: 1.0,
                latency_basis: "rtt".into(),
                points: vec![point(0.0), point(1.0)],
            },
            Duration::ZERO,
        );
        series.append(
            TimeSeries {
                interval_secs: 1.0,
                latency_basis: "response_time".into(),
                points: vec![point(0.0)],
            },
            Duration::from_millis(2_500),
        );
        let starts: Vec<f64> = series.points.iter().map(|p| p.start_secs).collect();
        assert_eq!(starts, [0.0, 1.0, 2.5]);
        assert_eq!(series.latency_basis, "mixed");
    }
}
