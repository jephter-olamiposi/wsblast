//! Wire conversions between local histogram metrics and coordinator protobuf payloads.
//!
//! Histograms travel as sparse `(value, count)` pairs so workers can report only the
//! latency values they observed; the coordinator reconstructs exact replicas before
//! merging, preserving percentile accuracy without shipping fixed bucket arrays.

use crate::distributed::proto::wsblast::{
    ErrorCategory as ProtoErrorCategory, HistogramData, WorkerMetrics as ProtoWorkerMetrics,
};
use crate::error::ErrorCategory;
use crate::metrics::{LiveMetrics, WorkerMetrics};
use hdrhistogram::Histogram;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;

/// Merges per-connection worker metrics into a single wire payload.
pub fn worker_metrics_to_proto(worker_id: &str, workers: &[WorkerMetrics]) -> ProtoWorkerMetrics {
    let mut handshake = BTreeMap::new();
    let mut rtt = BTreeMap::new();
    let mut errors: HashMap<i32, u64> = HashMap::new();
    let mut messages_sent = 0;
    let mut messages_recv = 0;
    let mut bytes_sent = 0;
    let mut bytes_recv = 0;
    let mut connections_established = 0;
    let mut connections_failed = 0;
    let mut reconnects = 0;

    for worker in workers {
        accumulate(&mut handshake, &worker.handshake_hist);
        accumulate(&mut rtt, &worker.rtt_hist);
        messages_sent += worker.messages_sent;
        messages_recv += worker.messages_recv;
        bytes_sent += worker.bytes_sent;
        bytes_recv += worker.bytes_recv;
        reconnects += worker.reconnects;

        for (category, count) in &worker.errors {
            *errors.entry(category_to_proto(*category)).or_insert(0) += count;
        }

        let snapshot = worker.live.snapshot();
        connections_established += snapshot.connections_established;
        connections_failed += snapshot.connections_failed;
    }

    ProtoWorkerMetrics {
        worker_id: worker_id.to_string(),
        handshake_histogram: vec![to_histogram_data("handshake_us", handshake)],
        rtt_histogram: vec![to_histogram_data("rtt_us", rtt)],
        messages_sent,
        messages_recv,
        bytes_sent,
        bytes_recv,
        errors,
        connections_established,
        connections_failed,
        reconnects,
    }
}

/// Reconstructs local metrics from a worker submission.
///
/// # Errors
///
/// Returns an error when a histogram carries mismatched bucket/count lengths or a
/// recorded value falls outside the supported latency bounds.
pub fn proto_to_worker_metrics(proto: &ProtoWorkerMetrics) -> Result<WorkerMetrics, String> {
    let mut metrics = WorkerMetrics::new(LiveMetrics::new()).map_err(|e| e.to_string())?;
    metrics.messages_sent = proto.messages_sent;
    metrics.messages_recv = proto.messages_recv;
    metrics.bytes_sent = proto.bytes_sent;
    metrics.bytes_recv = proto.bytes_recv;
    metrics.reconnects = proto.reconnects;

    for data in &proto.handshake_histogram {
        record_histogram_data(&mut metrics.handshake_hist, data)?;
    }
    for data in &proto.rtt_histogram {
        record_histogram_data(&mut metrics.rtt_hist, data)?;
    }
    for (value, count) in &proto.errors {
        if let Some(category) = category_from_proto(*value) {
            *metrics.errors.entry(category).or_insert(0) += count;
        }
    }

    let live = &metrics.live;
    live.connections_established
        .store(proto.connections_established, Ordering::Relaxed);
    live.connections_failed
        .store(proto.connections_failed, Ordering::Relaxed);
    live.reconnects.store(proto.reconnects, Ordering::Relaxed);
    live.messages_sent
        .store(proto.messages_sent, Ordering::Relaxed);
    live.messages_received
        .store(proto.messages_recv, Ordering::Relaxed);
    live.bytes_sent.store(proto.bytes_sent, Ordering::Relaxed);
    live.bytes_received
        .store(proto.bytes_recv, Ordering::Relaxed);

    Ok(metrics)
}

/// Merges histograms reported by several workers by recorded value.
pub fn merge_histogram_data(histograms: &[HistogramData]) -> Result<Option<HistogramData>, String> {
    let Some(first) = histograms.first() else {
        return Ok(None);
    };

    let mut merged = BTreeMap::new();
    for histogram in histograms {
        if histogram.buckets.len() != histogram.counts.len() {
            return Err(format!(
                "histogram '{}' has mismatched bucket and count lengths",
                histogram.name
            ));
        }
        for (value, count) in histogram.buckets.iter().zip(&histogram.counts) {
            *merged.entry(*value).or_insert(0) += count;
        }
    }

    Ok(Some(to_histogram_data(&first.name, merged)))
}

fn accumulate(into: &mut BTreeMap<u64, u64>, histogram: &Histogram<u64>) {
    for recorded in histogram.iter_recorded() {
        *into.entry(recorded.value_iterated_to()).or_insert(0) += recorded.count_at_value();
    }
}

fn to_histogram_data(name: &str, values: BTreeMap<u64, u64>) -> HistogramData {
    let (buckets, counts) = values.into_iter().unzip();
    HistogramData {
        name: name.to_string(),
        buckets,
        counts,
    }
}

fn record_histogram_data(target: &mut Histogram<u64>, data: &HistogramData) -> Result<(), String> {
    if data.buckets.len() != data.counts.len() {
        return Err(format!(
            "histogram '{}' has mismatched bucket and count lengths",
            data.name
        ));
    }
    for (value, count) in data.buckets.iter().zip(&data.counts) {
        target
            .record_n(*value, *count)
            .map_err(|e| format!("histogram '{}' rejected value {value}: {e}", data.name))?;
    }
    Ok(())
}

fn category_to_proto(category: ErrorCategory) -> i32 {
    use ErrorCategory as Local;
    let proto = match category {
        Local::DnsResolution => ProtoErrorCategory::DnsResolution,
        Local::TcpConnect => ProtoErrorCategory::TcpConnect,
        Local::TlsHandshake => ProtoErrorCategory::TlsHandshake,
        Local::HttpUpgradeRejected => ProtoErrorCategory::HttpUpgradeRejected,
        Local::ProtocolError => ProtoErrorCategory::ProtocolError,
        Local::WriteError => ProtoErrorCategory::WriteError,
        Local::ReadError => ProtoErrorCategory::ReadError,
        Local::Timeout => ProtoErrorCategory::Timeout,
        Local::UnexpectedClose => ProtoErrorCategory::UnexpectedClose,
        Local::Other => ProtoErrorCategory::Other,
    };
    proto as i32
}

fn category_from_proto(value: i32) -> Option<ErrorCategory> {
    use ErrorCategory as Local;
    let proto = ProtoErrorCategory::try_from(value).ok()?;
    Some(match proto {
        ProtoErrorCategory::DnsResolution => Local::DnsResolution,
        ProtoErrorCategory::TcpConnect => Local::TcpConnect,
        ProtoErrorCategory::TlsHandshake => Local::TlsHandshake,
        ProtoErrorCategory::HttpUpgradeRejected => Local::HttpUpgradeRejected,
        ProtoErrorCategory::ProtocolError => Local::ProtocolError,
        ProtoErrorCategory::WriteError => Local::WriteError,
        ProtoErrorCategory::ReadError => Local::ReadError,
        ProtoErrorCategory::Timeout => Local::Timeout,
        ProtoErrorCategory::UnexpectedClose => Local::UnexpectedClose,
        ProtoErrorCategory::Other => Local::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metrics() -> WorkerMetrics {
        let mut metrics = WorkerMetrics::new(LiveMetrics::new()).unwrap();
        metrics.handshake_hist.record(1_500).expect("in bounds");
        metrics.handshake_hist.record(2_500).expect("in bounds");
        metrics.rtt_hist.record(400).expect("in bounds");
        metrics.messages_sent = 10;
        metrics.messages_recv = 9;
        metrics.bytes_sent = 1_024;
        metrics.bytes_recv = 512;
        metrics.errors.insert(ErrorCategory::Timeout, 2);
        metrics
            .live
            .connections_established
            .store(1, Ordering::Relaxed);
        metrics
    }

    #[test]
    fn round_trip_preserves_latency_and_counters() {
        let workers = vec![sample_metrics(), sample_metrics()];
        let proto = worker_metrics_to_proto("worker-a", &workers);

        assert_eq!(proto.messages_sent, 20);
        assert_eq!(proto.messages_recv, 18);
        assert_eq!(proto.connections_established, 2);
        assert_eq!(
            proto.errors.get(&(ProtoErrorCategory::Timeout as i32)),
            Some(&4)
        );

        let restored = proto_to_worker_metrics(&proto).expect("valid wire metrics");
        assert_eq!(restored.handshake_hist.len(), 4);
        assert_eq!(restored.rtt_hist.len(), 2);
        assert_eq!(restored.messages_sent, 20);
        assert_eq!(
            restored.errors.get(&ErrorCategory::Timeout).copied(),
            Some(4)
        );
        assert_eq!(
            restored.live.snapshot().connections_established,
            2,
            "live counters survive the round trip"
        );
    }

    #[test]
    fn merges_histograms_with_disjoint_buckets() {
        let left = HistogramData {
            name: "rtt_us".into(),
            buckets: vec![100, 200],
            counts: vec![1, 1],
        };
        let right = HistogramData {
            name: "rtt_us".into(),
            buckets: vec![200, 300],
            counts: vec![2, 5],
        };

        let merged = merge_histogram_data(&[left, right])
            .expect("compatible histograms")
            .expect("non-empty merge");

        assert_eq!(merged.buckets, vec![100, 200, 300]);
        assert_eq!(merged.counts, vec![1, 3, 5]);
    }

    #[test]
    fn rejects_mismatched_histogram_lengths() {
        let broken = HistogramData {
            name: "rtt_us".into(),
            buckets: vec![100, 200],
            counts: vec![1],
        };
        assert!(merge_histogram_data(&[broken]).is_err());
    }
}
