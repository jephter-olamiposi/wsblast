//! Metric collection, lock-free telemetry, HDR latency histograms, and aggregation.

use crate::error::{ErrorCategory, Result as WsResult, WsBlastError};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Atomic counters queried during execution for live terminal and TUI progress updates.
#[derive(Debug, Default)]
pub struct LiveMetrics {
    pub active_connections: AtomicU64,
    /// Workers that completed their first handshake.
    pub connections_established: AtomicU64,
    /// Handshake attempts that failed, including failed reconnect attempts.
    pub connections_failed: AtomicU64,
    /// Handshakes that re-established a worker's connection after it was lost.
    pub reconnects: AtomicU64,
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub total_errors: AtomicU64,
    /// Data frames an echo wait skipped because they did not match the reply rule.
    pub unsolicited_frames: AtomicU64,
}

impl LiveMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Captures a point-in-time snapshot of atomic telemetry counters with relaxed consistency.
    pub fn snapshot(&self) -> LiveSnapshot {
        LiveSnapshot {
            active_connections: self.active_connections.load(Ordering::Relaxed),
            connections_established: self.connections_established.load(Ordering::Relaxed),
            connections_failed: self.connections_failed.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            total_errors: self.total_errors.load(Ordering::Relaxed),
            unsolicited_frames: self.unsolicited_frames.load(Ordering::Relaxed),
        }
    }
}

/// Instantaneous metric snapshot consumed by TUI and progress indicators.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveSnapshot {
    pub active_connections: u64,
    pub connections_established: u64,
    pub connections_failed: u64,
    pub reconnects: u64,
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub total_errors: u64,
    pub unsolicited_frames: u64,
}

/// Task-local metrics container owned by a single worker to eliminate mutex contention.
#[derive(Clone, Debug)]
pub struct WorkerMetrics {
    pub handshake_hist: Histogram<u64>,
    pub rtt_hist: Histogram<u64>,
    pub messages_sent: u64,
    pub messages_recv: u64,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub errors: HashMap<ErrorCategory, u64>,
    /// Handshakes recorded after this worker's first one; every other entry in
    /// `handshake_hist` is a first handshake.
    pub reconnects: u64,
    /// Data frames skipped while waiting for an echo reply.
    pub unsolicited_frames: u64,
    pub live: Arc<LiveMetrics>,
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
            messages_sent: 0,
            messages_recv: 0,
            bytes_sent: 0,
            bytes_recv: 0,
            errors: HashMap::new(),
            reconnects: 0,
            unsolicited_frames: 0,
            live,
        })
    }

    /// Records this worker's first completed handshake.
    #[inline]
    pub fn record_handshake(&mut self, duration: Duration) {
        self.record_handshake_latency(duration);
        self.live
            .connections_established
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a handshake that replaced a connection this worker had already
    /// established and lost.
    #[inline]
    pub fn record_reconnect(&mut self, duration: Duration) {
        self.record_handshake_latency(duration);
        self.reconnects += 1;
        self.live.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_handshake_latency(&mut self, duration: Duration) {
        let micros = duration.as_micros().clamp(1, 60_000_000) as u64;
        let _ = self.handshake_hist.record(micros);
        self.live.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_connection_failed(&mut self, err: ErrorCategory) {
        self.record_error(err);
        self.live.connections_failed.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_connection_closed(&mut self) {
        self.live.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_message_sent(&mut self, bytes: usize) {
        self.messages_sent += 1;
        self.bytes_sent += bytes as u64;
        self.live.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.live
            .bytes_sent
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_message_recv(&mut self, bytes: usize, rtt: Option<Duration>) {
        self.messages_recv += 1;
        self.bytes_recv += bytes as u64;
        if let Some(rtt_dur) = rtt {
            let micros = rtt_dur.as_micros().clamp(1, 60_000_000) as u64;
            let _ = self.rtt_hist.record(micros);
        }
        self.live.messages_received.fetch_add(1, Ordering::Relaxed);
        self.live
            .bytes_received
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_unsolicited_frames(&mut self, count: u64) {
        if count == 0 {
            return;
        }
        self.unsolicited_frames += count;
        self.live
            .unsolicited_frames
            .fetch_add(count, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_error(&mut self, err: ErrorCategory) {
        *self.errors.entry(err).or_insert(0) += 1;
        self.live.total_errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// Statistical summary computed from an HDR latency histogram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyStats {
    pub count: u64,
    pub min_us: u64,
    pub mean_us: f64,
    pub max_us: u64,
    pub std_dev_us: f64,
    pub p50_us: u64,
    pub p75_us: u64,
    pub p90_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub p9999_us: u64,
}

impl LatencyStats {
    pub fn from_histogram(hist: &Histogram<u64>) -> Self {
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

    pub fn p50_duration(&self) -> Duration {
        Duration::from_micros(self.p50_us)
    }

    pub fn p95_duration(&self) -> Duration {
        Duration::from_micros(self.p95_us)
    }

    pub fn p99_duration(&self) -> Duration {
        Duration::from_micros(self.p99_us)
    }

    pub fn p999_duration(&self) -> Duration {
        Duration::from_micros(self.p999_us)
    }
}

/// Consolidated metrics combining all worker sessions, transfer statistics, and rates.
///
/// Connection counts are per worker slot, so they never exceed the attempted
/// total even when workers reconnect:
/// - `total_connections_attempted`: worker slots the run tried to open.
/// - `total_connections_established`: slots that completed at least one handshake.
/// - `total_connections_failed`: slots that never completed a handshake.
/// - `total_reconnects`: extra handshakes that re-established a slot after its
///   connection was lost, so `handshake_latency.count` equals
///   `total_connections_established + total_reconnects`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedMetrics {
    pub total_connections_attempted: u64,
    pub total_connections_established: u64,
    pub total_connections_failed: u64,
    #[serde(default)]
    pub total_reconnects: u64,
    /// Data frames that did not match `--match` while an echo reply was awaited.
    #[serde(default)]
    pub total_unsolicited_frames: u64,
    pub total_messages_sent: u64,
    pub total_messages_recv: u64,
    pub total_bytes_sent: u64,
    pub total_bytes_recv: u64,
    pub elapsed: Duration,
    pub throughput_msg_per_sec: f64,
    pub throughput_bytes_per_sec: f64,
    pub error_rate: f64,
    pub handshake_latency: LatencyStats,
    pub message_rtt: LatencyStats,
    pub error_breakdown: HashMap<ErrorCategory, u64>,
}

impl AggregatedMetrics {
    /// Combines multiple worker metrics into a single unified summary.
    ///
    /// # Errors
    ///
    /// Returns [`WsBlastError::Metrics`] if the merged histogram configuration is
    /// rejected by the underlying implementation.
    pub fn merge(
        workers: Vec<WorkerMetrics>,
        elapsed: Duration,
        attempted_conns: u64,
    ) -> WsResult<Self> {
        let mut combined_handshake = latency_histogram()?;
        let mut combined_rtt = latency_histogram()?;

        let mut total_messages_sent = 0u64;
        let mut total_messages_recv = 0u64;
        let mut total_bytes_sent = 0u64;
        let mut total_bytes_recv = 0u64;
        let mut total_reconnects = 0u64;
        let mut total_unsolicited_frames = 0u64;
        let mut error_breakdown = HashMap::new();

        for w in workers {
            let _ = combined_handshake.add(&w.handshake_hist);
            let _ = combined_rtt.add(&w.rtt_hist);
            total_messages_sent += w.messages_sent;
            total_messages_recv += w.messages_recv;
            total_bytes_sent += w.bytes_sent;
            total_bytes_recv += w.bytes_recv;
            total_reconnects += w.reconnects;
            total_unsolicited_frames += w.unsolicited_frames;

            for (category, count) in w.errors {
                *error_breakdown.entry(category).or_insert(0) += count;
            }
        }

        let total_errors: u64 = error_breakdown.values().sum();
        let total_ops = total_messages_sent + total_errors;
        let error_rate = if total_ops > 0 {
            total_errors as f64 / total_ops as f64
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
        let total_connections_failed =
            attempted_conns.saturating_sub(total_connections_established);

        Ok(Self {
            total_connections_attempted: attempted_conns,
            total_connections_established,
            total_connections_failed,
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
            error_breakdown,
        })
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
