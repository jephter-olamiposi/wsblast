//! Per-worker HDR latency histograms and counters, the relaxed atomics behind
//! live progress, and the merge into [`AggregatedMetrics`].

use crate::error::{ErrorCategory, Result as WsResult, WsBlastError};
use crate::timeseries::{IntervalRecorder, TimeSeries};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// One shard of the live counters, aligned to its own cache lines so that
/// workers updating different shards do not contend for the same line.
#[derive(Debug, Default)]
#[repr(align(128))]
pub struct CounterShard {
    /// Connections currently open.
    pub active_connections: AtomicU64,
    /// Workers that completed their first handshake.
    pub connections_established: AtomicU64,
    /// Handshake attempts that failed, including failed reconnect attempts.
    pub connections_failed: AtomicU64,
    /// Handshakes that re-established a worker's connection after it was lost.
    pub reconnects: AtomicU64,
    /// Data frames sent.
    pub messages_sent: AtomicU64,
    /// Data frames received.
    pub messages_received: AtomicU64,
    /// Payload bytes sent.
    pub bytes_sent: AtomicU64,
    /// Payload bytes received.
    pub bytes_received: AtomicU64,
    /// Errors of every category.
    pub total_errors: AtomicU64,
    /// Data frames an echo wait skipped because they did not match the reply rule.
    pub unsolicited_frames: AtomicU64,
}

/// A request-limit counter on its own cache lines.
#[derive(Debug, Default)]
#[repr(align(128))]
struct ClaimCounter(AtomicU64);

/// Live counters of a run, read for progress, the TUI and the time series.
///
/// Every message updates several counters. If all workers updated the same
/// atomics, every update would move their cache line between CPU cores, so
/// the counters are split into shards and each worker writes to one of them.
/// [`LiveMetrics::snapshot`] adds the shards up.
#[derive(Debug)]
pub struct LiveMetrics {
    shards: Box<[CounterShard]>,
    next_shard: AtomicUsize,
    /// Sends claimed against `--requests`; touched only when a limit is set.
    requests_claimed: ClaimCounter,
}

impl Default for LiveMetrics {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            shards: (0..cores * 2).map(|_| CounterShard::default()).collect(),
            next_shard: AtomicUsize::new(0),
            requests_claimed: ClaimCounter::default(),
        }
    }
}

impl LiveMetrics {
    /// Returns a zeroed registry ready to share between workers.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Picks the shard for a new worker, round robin.
    fn assign_shard(&self) -> usize {
        self.next_shard.fetch_add(1, Ordering::Relaxed) % self.shards.len()
    }

    /// The counters of shard `index`.
    pub fn shard(&self, index: usize) -> &CounterShard {
        &self.shards[index % self.shards.len()]
    }

    /// Claims one send against a request limit of `max`. Returns false once
    /// `max` sends have been claimed, so the run never sends more than `max`
    /// messages in total.
    pub(crate) fn claim_request(&self, max: u64) -> bool {
        self.requests_claimed.0.fetch_add(1, Ordering::Relaxed) < max
    }

    /// Whether `max` sends have been claimed.
    pub(crate) fn requests_exhausted(&self, max: u64) -> bool {
        self.requests_claimed.0.load(Ordering::Relaxed) >= max
    }

    /// Captures a point-in-time snapshot by adding up every shard, with
    /// relaxed consistency.
    pub fn snapshot(&self) -> LiveSnapshot {
        let mut snap = LiveSnapshot::default();
        for shard in &*self.shards {
            let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            snap.active_connections += load(&shard.active_connections);
            snap.connections_established += load(&shard.connections_established);
            snap.connections_failed += load(&shard.connections_failed);
            snap.reconnects += load(&shard.reconnects);
            snap.messages_sent += load(&shard.messages_sent);
            snap.messages_received += load(&shard.messages_received);
            snap.bytes_sent += load(&shard.bytes_sent);
            snap.bytes_received += load(&shard.bytes_received);
            snap.total_errors += load(&shard.total_errors);
            snap.unsolicited_frames += load(&shard.unsolicited_frames);
        }
        snap
    }
}

/// Point-in-time view of the live counters of a run in progress.
///
/// The counters are read one by one with relaxed ordering, so a snapshot taken
/// while workers run can be slightly inconsistent across fields. The final
/// report comes from [`AggregatedMetrics`], not from these counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LiveSnapshot {
    /// Connections currently open.
    pub active_connections: u64,
    /// Workers that completed their first handshake.
    pub connections_established: u64,
    /// Failed handshake attempts, including failed reconnect attempts.
    pub connections_failed: u64,
    /// Handshakes that re-established a lost connection.
    pub reconnects: u64,
    /// Data frames sent.
    pub messages_sent: u64,
    /// Data frames received.
    pub messages_received: u64,
    /// Payload bytes sent.
    pub bytes_sent: u64,
    /// Payload bytes received.
    pub bytes_received: u64,
    /// Errors of every category.
    pub total_errors: u64,
    /// Data frames an echo wait skipped because they did not match the reply rule.
    pub unsolicited_frames: u64,
}

/// How a recorded error relates to the attempted operations that form the
/// error-rate denominator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorScope {
    /// The failed operation never produced a sent message (a handshake, a
    /// write, a stream read), so it counts as an attempted operation itself.
    Attempt,
    /// The error ends a message that is already counted as sent (a reply
    /// timeout, a read error or a close while the reply is awaited), so it
    /// adds no second attempt.
    SentMessage,
}

/// Task-local metrics container owned by a single worker to eliminate mutex contention.
#[derive(Clone, Debug)]
pub struct WorkerMetrics {
    pub(crate) handshake_hist: Histogram<u64>,
    pub(crate) rtt_hist: Histogram<u64>,
    /// Paced echo latency measured from each request's scheduled send time.
    /// Created on the first sample, so unpaced runs do not pay for it.
    pub(crate) response_hist: Option<Histogram<u64>>,
    pub(crate) messages_sent: u64,
    pub(crate) messages_recv: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) bytes_recv: u64,
    pub(crate) errors: HashMap<ErrorCategory, u64>,
    /// Errors in `errors` recorded with [`ErrorScope::SentMessage`].
    pub(crate) sent_message_errors: u64,
    /// Handshakes recorded after this worker's first one; every other entry in
    /// `handshake_hist` is a first handshake.
    pub(crate) reconnects: u64,
    /// Data frames skipped while waiting for an echo reply.
    pub(crate) unsolicited_frames: u64,
    /// The run ended during this worker's first handshake, before it
    /// succeeded or failed.
    pub(crate) interrupted: bool,
    /// Latency samples for the run's time series, when it keeps one.
    pub(crate) interval: Option<IntervalRecorder>,
    /// Whether the time series takes response time instead of RTT, as it
    /// does for paced echo sessions.
    pub(crate) interval_uses_response: bool,
    pub(crate) live: Arc<LiveMetrics>,
    /// The live counter shard this worker updates.
    shard: usize,
}

fn latency_histogram() -> WsResult<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000, 3)
        .map_err(|e| WsBlastError::Metrics(format!("invalid latency histogram bounds: {e}")))
}

impl WorkerMetrics {
    /// Initializes histograms covering 1 µs to 60s at 3 significant figures.
    ///
    /// The fixed bounds prevent memory reallocations on the latency measurement hot path.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Metrics`] if the histogram configuration is rejected
    /// by the underlying implementation.
    pub fn new(live: Arc<LiveMetrics>) -> WsResult<Self> {
        let handshake_hist = latency_histogram()?;
        let rtt_hist = latency_histogram()?;

        Ok(Self {
            handshake_hist,
            rtt_hist,
            response_hist: None,
            messages_sent: 0,
            messages_recv: 0,
            bytes_sent: 0,
            bytes_recv: 0,
            errors: HashMap::new(),
            sent_message_errors: 0,
            reconnects: 0,
            unsolicited_frames: 0,
            interrupted: false,
            interval: None,
            interval_uses_response: false,
            shard: live.assign_shard(),
            live,
        })
    }

    /// The live counter shard this worker updates.
    #[inline]
    fn counters(&self) -> &CounterShard {
        self.live.shard(self.shard)
    }

    /// Sends this worker's latency samples to the run's time series.
    pub(crate) fn with_interval(mut self, interval: Option<IntervalRecorder>) -> Self {
        self.interval = interval;
        self
    }

    /// Hands the samples of the worker's current interval to the time series.
    pub(crate) fn flush_interval(&mut self) {
        if let Some(interval) = self.interval.as_mut() {
            interval.flush();
        }
    }

    /// Records this worker's first completed handshake.
    #[inline]
    pub fn record_handshake(&mut self, duration: Duration) {
        self.record_handshake_latency(duration);
        self.counters()
            .connections_established
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a handshake that replaced a connection this worker had already
    /// established and lost.
    #[inline]
    pub(crate) fn record_reconnect(&mut self, duration: Duration) {
        self.record_handshake_latency(duration);
        self.reconnects += 1;
        self.counters().reconnects.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_handshake_latency(&mut self, duration: Duration) {
        let micros = duration.as_micros().clamp(1, 60_000_000) as u64;
        let _ = self.handshake_hist.record(micros);
        self.counters()
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn record_connection_failed(&mut self, err: ErrorCategory) {
        self.record_error(err, ErrorScope::Attempt);
        self.counters()
            .connections_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn record_connection_closed(&mut self) {
        self.counters()
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn record_message_sent(&mut self, bytes: usize) {
        self.messages_sent += 1;
        self.bytes_sent += bytes as u64;
        self.counters()
            .messages_sent
            .fetch_add(1, Ordering::Relaxed);
        self.counters()
            .bytes_sent
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Records a received data frame and, in echo mode, its round-trip time.
    #[inline]
    pub fn record_message_recv(&mut self, bytes: usize, rtt: Option<Duration>) {
        self.messages_recv += 1;
        self.bytes_recv += bytes as u64;
        if let Some(rtt_dur) = rtt {
            let micros = rtt_dur.as_micros().clamp(1, 60_000_000) as u64;
            let _ = self.rtt_hist.record(micros);
            if !self.interval_uses_response
                && let Some(interval) = self.interval.as_mut()
            {
                interval.record(micros);
            }
        }
        self.counters()
            .messages_received
            .fetch_add(1, Ordering::Relaxed);
        self.counters()
            .bytes_received
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Records the time from a paced request's scheduled send time to its
    /// reply.
    #[inline]
    pub(crate) fn record_response_time(&mut self, duration: Duration) {
        let micros = duration.as_micros().clamp(1, 60_000_000) as u64;
        if self.response_hist.is_none() {
            self.response_hist = latency_histogram().ok();
        }
        if let Some(hist) = self.response_hist.as_mut() {
            let _ = hist.record(micros);
        }
        if let Some(interval) = self.interval.as_mut() {
            interval.record(micros);
        }
    }

    #[inline]
    pub(crate) fn record_unsolicited_frames(&mut self, count: u64) {
        if count == 0 {
            return;
        }
        self.unsolicited_frames += count;
        self.counters()
            .unsolicited_frames
            .fetch_add(count, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn record_error(&mut self, err: ErrorCategory, scope: ErrorScope) {
        *self.errors.entry(err).or_insert(0) += 1;
        if scope == ErrorScope::SentMessage {
            self.sent_message_errors += 1;
        }
        self.counters().total_errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// Summary of one latency distribution, in microseconds.
///
/// Values come from an HDR histogram covering 1 µs to 60 s at three
/// significant figures. With no samples every field is zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LatencyStats {
    /// Number of samples.
    pub count: u64,
    /// Smallest sample.
    pub min_us: u64,
    /// Arithmetic mean.
    pub mean_us: f64,
    /// Largest sample.
    pub max_us: u64,
    /// Standard deviation.
    pub std_dev_us: f64,
    /// Median.
    pub p50_us: u64,
    /// 75th percentile.
    pub p75_us: u64,
    /// 90th percentile.
    pub p90_us: u64,
    /// 95th percentile.
    pub p95_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// 99.9th percentile.
    pub p999_us: u64,
    /// 99.99th percentile.
    pub p9999_us: u64,
}

impl LatencyStats {
    pub(crate) fn from_histogram(hist: &Histogram<u64>) -> Self {
        if hist.is_empty() {
            return Self {
                count: 0,
                min_us: 0,
                mean_us: 0.0,
                max_us: 0,
                std_dev_us: 0.0,
                p50_us: 0,
                p75_us: 0,
                p90_us: 0,
                p95_us: 0,
                p99_us: 0,
                p999_us: 0,
                p9999_us: 0,
            };
        }

        Self {
            count: hist.len(),
            min_us: hist.min(),
            mean_us: hist.mean(),
            max_us: hist.max(),
            std_dev_us: hist.stdev(),
            p50_us: hist.value_at_quantile(0.50),
            p75_us: hist.value_at_quantile(0.75),
            p90_us: hist.value_at_quantile(0.90),
            p95_us: hist.value_at_quantile(0.95),
            p99_us: hist.value_at_quantile(0.99),
            p999_us: hist.value_at_quantile(0.999),
            p9999_us: hist.value_at_quantile(0.9999),
        }
    }

    /// Median as a [`Duration`].
    pub fn p50_duration(&self) -> Duration {
        Duration::from_micros(self.p50_us)
    }

    /// 95th percentile as a [`Duration`].
    pub fn p95_duration(&self) -> Duration {
        Duration::from_micros(self.p95_us)
    }

    /// 99th percentile as a [`Duration`].
    pub fn p99_duration(&self) -> Duration {
        Duration::from_micros(self.p99_us)
    }

    /// 99.9th percentile as a [`Duration`].
    pub fn p999_duration(&self) -> Duration {
        Duration::from_micros(self.p999_us)
    }
}

/// Final metrics of a run, merged from every worker.
///
/// Connection counts are per worker slot, so they never exceed the attempted
/// total even when workers reconnect:
/// - `total_connections_attempted`: worker slots the run tried to open.
/// - `total_connections_established`: slots that completed at least one handshake.
/// - `total_connections_failed`: started slots that never completed a handshake
///   and recorded at least one connection error.
/// - `total_connections_interrupted`: started slots whose first handshake was
///   still in progress when the run ended, so they neither succeeded nor
///   failed.
/// - `total_connections_not_started`: slots the ramp had not reached when the
///   run ended, so `total_connections_attempted` equals
///   `total_connections_established + total_connections_failed +
///   total_connections_interrupted + total_connections_not_started`.
/// - `total_reconnects`: extra handshakes that re-established a slot after its
///   connection was lost, so `handshake_latency.count` equals
///   `total_connections_established + total_reconnects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AggregatedMetrics {
    /// Connection slots the run was configured to open.
    pub total_connections_attempted: u64,
    /// Slots that completed at least one handshake.
    pub total_connections_established: u64,
    /// Started slots that never completed a handshake and recorded a
    /// connection error.
    pub total_connections_failed: u64,
    /// Started slots whose first handshake was still in progress when the
    /// run ended.
    #[serde(default)]
    pub total_connections_interrupted: u64,
    /// Slots the ramp had not reached when the run ended.
    #[serde(default)]
    pub total_connections_not_started: u64,
    /// Handshakes that re-established a slot after its connection was lost.
    #[serde(default)]
    pub total_reconnects: u64,
    /// Data frames that did not match `--match` while an echo reply was awaited.
    #[serde(default)]
    pub total_unsolicited_frames: u64,
    /// Data frames sent.
    pub total_messages_sent: u64,
    /// Data frames received.
    pub total_messages_recv: u64,
    /// Payload bytes sent.
    pub total_bytes_sent: u64,
    /// Payload bytes received.
    pub total_bytes_recv: u64,
    /// Wall-clock run time, including the connection ramp.
    pub elapsed: Duration,
    /// The larger of messages sent and received, per second of `elapsed`.
    pub throughput_msg_per_sec: f64,
    /// Bytes sent plus bytes received, per second of `elapsed`.
    pub throughput_bytes_per_sec: f64,
    /// Failed operations over attempted operations, between 0.0 and 1.0.
    pub error_rate: f64,
    /// Time from connect to a completed WebSocket handshake.
    pub handshake_latency: LatencyStats,
    /// Echo round-trip time, from the actual send to the reply; empty
    /// outside echo mode.
    pub message_rtt: LatencyStats,
    /// Echo response time, from each request's scheduled send time to its
    /// reply. Recorded only for paced echo runs (`--rate`), where it
    /// includes the time a request waited behind a slow reply; empty
    /// otherwise.
    #[serde(default)]
    pub response_time: LatencyStats,
    /// The run broken down by time: counters, throughput and latency per
    /// interval.
    #[serde(default)]
    pub time_series: TimeSeries,
    /// Error counts by category.
    pub error_breakdown: HashMap<ErrorCategory, u64>,
}

impl AggregatedMetrics {
    /// Combines multiple worker metrics into a single unified summary.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Metrics`] if the merged histogram configuration is
    /// rejected by the underlying implementation.
    pub(crate) fn merge(
        workers: Vec<WorkerMetrics>,
        elapsed: Duration,
        attempted_conns: u64,
    ) -> WsResult<Self> {
        let mut combined_handshake = latency_histogram()?;
        let mut combined_rtt = latency_histogram()?;
        let mut combined_response = latency_histogram()?;

        let mut total_messages_sent = 0u64;
        let mut total_messages_recv = 0u64;
        let mut total_bytes_sent = 0u64;
        let mut total_bytes_recv = 0u64;
        let mut total_reconnects = 0u64;
        let mut total_unsolicited_frames = 0u64;
        let mut total_connections_interrupted = 0u64;
        let mut sent_message_errors = 0u64;
        let mut error_breakdown = HashMap::new();

        for w in workers {
            let _ = combined_handshake.add(&w.handshake_hist);
            let _ = combined_rtt.add(&w.rtt_hist);
            if let Some(hist) = &w.response_hist {
                let _ = combined_response.add(hist);
            }
            total_messages_sent += w.messages_sent;
            total_messages_recv += w.messages_recv;
            total_bytes_sent += w.bytes_sent;
            total_bytes_recv += w.bytes_recv;
            total_reconnects += w.reconnects;
            total_unsolicited_frames += w.unsolicited_frames;
            sent_message_errors += w.sent_message_errors;
            total_connections_interrupted += u64::from(w.interrupted);

            for (category, count) in w.errors {
                *error_breakdown.entry(category).or_insert(0) += count;
            }
        }

        // Failed operations over attempted operations: every sent message, plus
        // every failure that happened before a message could be sent. An error
        // that ends an already-sent message is a failure but not a new attempt.
        let total_errors: u64 = error_breakdown.values().sum();
        let attempted_ops = total_messages_sent + total_errors.saturating_sub(sent_message_errors);
        let error_rate = if attempted_ops > 0 {
            (total_errors as f64 / attempted_ops as f64).min(1.0)
        } else {
            0.0
        };

        let elapsed_secs = elapsed.as_secs_f64().max(0.0001);
        // Listen mode never sends, and stream mode may send without replies; using
        // the larger direction keeps throughput meaningful for every mode.
        let throughput_msg_per_sec =
            total_messages_sent.max(total_messages_recv) as f64 / elapsed_secs;
        let throughput_bytes_per_sec = (total_bytes_sent + total_bytes_recv) as f64 / elapsed_secs;

        let total_connections_established = combined_handshake
            .len()
            .saturating_sub(total_reconnects)
            .min(attempted_conns);
        let total_connections_interrupted = total_connections_interrupted
            .min(attempted_conns.saturating_sub(total_connections_established));
        let total_connections_failed = attempted_conns
            .saturating_sub(total_connections_established)
            .saturating_sub(total_connections_interrupted);

        Ok(Self {
            total_connections_attempted: attempted_conns,
            total_connections_established,
            total_connections_failed,
            total_connections_interrupted,
            total_connections_not_started: 0,
            total_reconnects,
            total_unsolicited_frames,
            total_messages_sent,
            total_messages_recv,
            total_bytes_sent,
            total_bytes_recv,
            elapsed,
            throughput_msg_per_sec,
            throughput_bytes_per_sec,
            error_rate,
            handshake_latency: LatencyStats::from_histogram(&combined_handshake),
            message_rtt: LatencyStats::from_histogram(&combined_rtt),
            response_time: LatencyStats::from_histogram(&combined_response),
            time_series: TimeSeries::default(),
            error_breakdown,
        })
    }

    /// Attaches the run's time series.
    #[must_use]
    pub(crate) fn with_time_series(mut self, time_series: TimeSeries) -> Self {
        self.time_series = time_series;
        self
    }

    /// Reclassifies `not_started` slots, which have no worker metrics, from
    /// failed to not started. [`AggregatedMetrics::merge`] counts every slot
    /// without a handshake as failed because it only sees started workers.
    #[must_use]
    pub(crate) fn with_not_started(mut self, not_started: u64) -> Self {
        let not_started = not_started.min(self.total_connections_failed);
        self.total_connections_failed -= not_started;
        self.total_connections_not_started = not_started;
        self
    }
}

#[cfg(test)]
mod aggregation_tests {
    use super::*;

    #[test]
    fn reconnect_handshakes_do_not_inflate_established_connections() {
        let live = LiveMetrics::new();
        let mut first = WorkerMetrics::new(Arc::clone(&live)).expect("worker metrics");
        first.record_handshake(Duration::from_millis(1));
        first.record_connection_closed();
        first.record_reconnect(Duration::from_millis(2));
        first.record_connection_closed();
        first.record_reconnect(Duration::from_millis(3));
        first.record_connection_closed();
        let mut never_connected = WorkerMetrics::new(Arc::clone(&live)).expect("worker metrics");
        never_connected.record_connection_failed(ErrorCategory::TcpConnect);
        never_connected.record_connection_failed(ErrorCategory::TcpConnect);

        let merged =
            AggregatedMetrics::merge(vec![first, never_connected], Duration::from_secs(1), 2)
                .expect("merge");

        assert_eq!(merged.total_connections_attempted, 2);
        assert_eq!(merged.total_connections_established, 1);
        assert_eq!(merged.total_connections_failed, 1);
        assert_eq!(merged.total_reconnects, 2);
        assert_eq!(merged.handshake_latency.count, 3);

        let snapshot = live.snapshot();
        assert_eq!(snapshot.active_connections, 0);
        assert_eq!(snapshot.connections_established, 1);
        assert_eq!(snapshot.connections_failed, 2);
        assert_eq!(snapshot.reconnects, 2);
    }

    fn error_rate_of(record: impl FnOnce(&mut WorkerMetrics)) -> f64 {
        let mut worker = WorkerMetrics::new(LiveMetrics::new()).expect("worker metrics");
        record(&mut worker);
        AggregatedMetrics::merge(vec![worker], Duration::from_secs(1), 1)
            .expect("merge")
            .error_rate
    }

    #[test]
    fn error_rate_is_failed_over_attempted_operations() {
        let all_timed_out = error_rate_of(|w| {
            for _ in 0..4 {
                w.record_message_sent(10);
                w.record_error(ErrorCategory::Timeout, ErrorScope::SentMessage);
            }
        });
        assert_eq!(all_timed_out, 1.0);

        let clean = error_rate_of(|w| {
            for _ in 0..5 {
                w.record_message_sent(10);
                w.record_message_recv(10, Some(Duration::from_millis(1)));
            }
        });
        assert_eq!(clean, 0.0);

        let refused = error_rate_of(|w| {
            for _ in 0..3 {
                w.record_connection_failed(ErrorCategory::TcpConnect);
            }
        });
        assert_eq!(refused, 1.0);

        // 8 sent messages (one times out, one hits a read error while its
        // reply is awaited), 1 failed handshake and 1 failed write: 4 failures
        // over 10 attempted operations.
        let mixed = error_rate_of(|w| {
            w.record_connection_failed(ErrorCategory::TcpConnect);
            for _ in 0..8 {
                w.record_message_sent(10);
            }
            w.record_error(ErrorCategory::Timeout, ErrorScope::SentMessage);
            w.record_error(ErrorCategory::ReadError, ErrorScope::SentMessage);
            w.record_error(ErrorCategory::WriteError, ErrorScope::Attempt);
        });
        assert_eq!(mixed, 0.4);
    }

    #[test]
    fn reports_without_reconnect_field_still_parse() {
        let merged =
            AggregatedMetrics::merge(Vec::new(), Duration::from_secs(1), 0).expect("merge");
        let mut value = serde_json::to_value(&merged).expect("serialize");
        value
            .as_object_mut()
            .expect("object")
            .remove("total_reconnects");

        let parsed: AggregatedMetrics = serde_json::from_value(value).expect("legacy report");
        assert_eq!(parsed.total_reconnects, 0);
    }
}
