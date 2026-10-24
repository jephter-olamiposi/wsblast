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

fn interval_histogram() -> Option<Histogram<u64>> {
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

    fn absorb(&self, bucket: u64, hist: &Histogram<u64>) {
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
}

impl CounterSampler {
    /// Starts sampling `live` every `interval` from `start` until `cancel`
    /// fires, then takes one last sample at the end of the run.
    pub(crate) fn spawn(
        live: Arc<LiveMetrics>,
        start: Instant,
        interval: Duration,
        cancel: CancellationToken,
    ) -> Self {
        let task = tokio::spawn(async move {
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
            samples.push((start.elapsed(), live.snapshot()));
            samples
        });
        Self { task }
    }

    /// Returns the samples once the run has ended.
    pub(crate) async fn finish(self) -> Vec<(Duration, LiveSnapshot)> {
        self.task.await.unwrap_or_default()
    }
}

/// Metrics of a run broken down by time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TimeSeries {
    /// Length of each interval in seconds. The last point of a run, or of a
    /// scenario phase, may be shorter.
    pub interval_secs: f64,
    /// The latency each point reports: `response_time` for paced echo runs,
    /// `rtt` otherwise.
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
    fn from_histogram(hist: Option<&Histogram<u64>>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

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
