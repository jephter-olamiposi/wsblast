//! Metric collection, lock-free telemetry, HDR latency histograms, and aggregation.

use crate::error::{ErrorCategory, Result as WsResult, WsBlastError};
#[cfg(feature = "capture")]
use bytes::Bytes;
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(feature = "capture")]
use std::fs::File;
#[cfg(feature = "capture")]
use std::io::BufWriter;

#[cfg(feature = "capture")]
use crate::config::CaptureFormat;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(feature = "capture")]
use crossbeam::queue::SegQueue;
#[cfg(feature = "capture")]
use tokio_tungstenite::tungstenite::protocol::Message;

#[cfg(feature = "otel")]
use opentelemetry::metrics::{Counter, Gauge, Histogram as OtelHistogram, MeterProvider as _};
#[cfg(feature = "otel")]
use opentelemetry_otlp::WithExportConfig;
#[cfg(feature = "otel")]
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
#[cfg(feature = "otel")]
use opentelemetry_sdk::runtime::Tokio;
#[cfg(feature = "prometheus")]
use prometheus_client::metrics::{
    counter::Counter as PromCounter,
    gauge::Gauge as PromGauge,
    histogram::{Histogram as PromHistogram, exponential_buckets},
};
#[cfg(feature = "prometheus")]
use prometheus_client::registry::Registry;

/// Atomic counters queried during execution for live terminal and TUI progress updates.
#[derive(Debug, Default)]
pub struct LiveMetrics {
    pub active_connections: AtomicU64,
    pub connections_established: AtomicU64,
    pub connections_failed: AtomicU64,
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub total_errors: AtomicU64,
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
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            total_errors: self.total_errors.load(Ordering::Relaxed),
        }
    }
}

/// Instantaneous metric snapshot consumed by TUI and progress indicators.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveSnapshot {
    pub active_connections: u64,
    pub connections_established: u64,
    pub connections_failed: u64,
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub total_errors: u64,
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
            live,
        })
    }

    #[inline]
    pub fn record_handshake(&mut self, duration: Duration) {
        let micros = duration.as_micros().clamp(1, 60_000_000) as u64;
        let _ = self.handshake_hist.record(micros);
        self.live
            .connections_established
            .fetch_add(1, Ordering::Relaxed);
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
    pub fn record_error(&mut self, err: ErrorCategory) {
        *self.errors.entry(err).or_insert(0) += 1;
        self.live.total_errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// Frame capture for recording WebSocket frames to PCAP-NG or JSONL files.
/// Uses a lock-free ring buffer (SegQueue) for zero-copy hot-path recording.
#[cfg(feature = "capture")]
#[derive(Clone)]
pub struct FrameCapture {
    frames: Arc<SegQueue<CapturedFrame>>,
    max_frames: usize,
}

#[cfg(feature = "capture")]
#[derive(Debug, Clone)]
struct CapturedFrame {
    timestamp_ms: u64,
    worker_id: usize,
    direction: CaptureDirection,
    payload: Bytes,
    is_binary: bool,
}

#[cfg(feature = "capture")]
#[derive(Debug, Clone, Copy)]
enum CaptureDirection {
    Sent,
    Received,
}

#[cfg(feature = "capture")]
impl FrameCapture {
    pub fn new(max_frames: usize) -> Self {
        Self {
            frames: Arc::new(SegQueue::new()),
            max_frames,
        }
    }

    #[inline]
    pub fn record_sent(&self, worker_id: usize, payload: &Message) {
        if self.max_frames != 0 && self.frames.len() >= self.max_frames {
            return; // Stop recording once the configured cap is reached.
        }
        let frame = CapturedFrame {
            timestamp_ms: current_timestamp_ms(),
            worker_id,
            direction: CaptureDirection::Sent,
            payload: message_to_bytes(payload),
            is_binary: payload.is_binary(),
        };
        self.frames.push(frame);
    }

    #[inline]
    pub fn record_received(&self, worker_id: usize, payload: &Message) {
        if self.max_frames != 0 && self.frames.len() >= self.max_frames {
            return;
        }
        let frame = CapturedFrame {
            timestamp_ms: current_timestamp_ms(),
            worker_id,
            direction: CaptureDirection::Received,
            payload: message_to_bytes(payload),
            is_binary: payload.is_binary(),
        };
        self.frames.push(frame);
    }

    /// Drains all captured frames and writes to file in specified format.
    pub fn drain_and_write(
        &self,
        path: &std::path::Path,
        format: CaptureFormat,
    ) -> std::io::Result<()> {
        let mut frames = Vec::new();
        while let Some(frame) = self.frames.pop() {
            frames.push(frame);
        }
        match format {
            CaptureFormat::PcapNg => write_pcapng(path, &frames),
            CaptureFormat::JsonLines => write_jsonl(path, &frames),
        }
    }
}

#[cfg(feature = "capture")]
fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(feature = "capture")]
fn message_to_bytes(msg: &Message) -> Bytes {
    match msg {
        Message::Text(t) => Bytes::copy_from_slice(t.as_bytes()),
        Message::Binary(b) => b.clone(),
        Message::Ping(p) | Message::Pong(p) => Bytes::copy_from_slice(p),
        Message::Close(c) => {
            let data = format!("{:?}", c);
            Bytes::from(data)
        }
        Message::Frame(f) => {
            let data = format!("{:?}", f);
            Bytes::from(data)
        }
    }
}

#[cfg(feature = "capture")]
fn write_jsonl(path: &std::path::Path, frames: &[CapturedFrame]) -> std::io::Result<()> {
    use base64::Engine as _;
    use std::io::Write;
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for frame in frames {
        let json = serde_json::json!({
            "timestamp_ms": frame.timestamp_ms,
            "worker_id": frame.worker_id,
            "direction": match frame.direction {
                CaptureDirection::Sent => "sent",
                CaptureDirection::Received => "received",
            },
            "is_binary": frame.is_binary,
            "payload": base64::engine::general_purpose::STANDARD.encode(&frame.payload),
            "payload_len": frame.payload.len(),
        });
        writeln!(writer, "{}", json)?;
    }
    Ok(())
}
#[cfg(feature = "capture")]
fn write_pcapng(path: &std::path::Path, frames: &[CapturedFrame]) -> std::io::Result<()> {
    use pcap_file::DataLink;
    use pcap_file::pcapng::PcapNgBlock;
    use pcap_file::pcapng::PcapNgWriter;
    use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
    use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
    use std::borrow::Cow;
    use std::time::Duration;

    let file = File::create(path)?;
    let mut writer = PcapNgWriter::new(BufWriter::new(file))
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // Write Section Header Block (already written by PcapNgWriter::new)

    // Write Interface Description Block
    let idb = InterfaceDescriptionBlock::new(DataLink::NULL, 65535);
    writer
        .write_block(&idb.into_block())
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    for frame in frames {
        let timestamp = Duration::from_micros(frame.timestamp_ms * 1000);
        let packet_data: Cow<'_, [u8]> = Cow::Owned(frame.payload.to_vec());

        let mut epb = EnhancedPacketBlock::default();
        epb.interface_id = 0;
        epb.timestamp = timestamp;
        epb.original_len = packet_data.len() as u32;
        epb.data = packet_data;
        epb.options = Vec::new();
        writer
            .write_block(&epb.into_block())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }

    Ok(())
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedMetrics {
    pub total_connections_attempted: u64,
    pub total_connections_established: u64,
    pub total_connections_failed: u64,
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
        let mut error_breakdown = HashMap::new();

        for w in workers {
            let _ = combined_handshake.add(&w.handshake_hist);
            let _ = combined_rtt.add(&w.rtt_hist);
            total_messages_sent += w.messages_sent;
            total_messages_recv += w.messages_recv;
            total_bytes_sent += w.bytes_sent;
            total_bytes_recv += w.bytes_recv;

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

        let total_connections_established = combined_handshake.len();
        let total_connections_failed =
            attempted_conns.saturating_sub(total_connections_established);

        Ok(Self {
            total_connections_attempted: attempted_conns,
            total_connections_established,
            total_connections_failed,
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

#[cfg(any(feature = "otel", feature = "prometheus"))]
fn counter_delta(last: &AtomicU64, current: u64) -> u64 {
    let previous = last.swap(current, Ordering::Relaxed);
    current.saturating_sub(previous)
}

/// OpenTelemetry metrics exporter for pushing to OTLP endpoints (e.g., Jaeger, Tempo, Grafana Cloud).
#[cfg(feature = "otel")]
pub struct OtelExporter {
    provider: SdkMeterProvider,
    _meter: opentelemetry::metrics::Meter,
    handshake_histogram: OtelHistogram<u64>,
    rtt_histogram: OtelHistogram<u64>,
    messages_sent_counter: Counter<u64>,
    messages_received_counter: Counter<u64>,
    bytes_sent_counter: Counter<u64>,
    bytes_received_counter: Counter<u64>,
    active_connections_gauge: Gauge<i64>,
    connections_established_counter: Counter<u64>,
    connections_failed_counter: Counter<u64>,
    total_errors_counter: Counter<u64>,
    last_messages_sent: AtomicU64,
    last_messages_received: AtomicU64,
    last_bytes_sent: AtomicU64,
    last_bytes_received: AtomicU64,
    last_connections_established: AtomicU64,
    last_connections_failed: AtomicU64,
    last_total_errors: AtomicU64,
}

#[cfg(feature = "otel")]
impl OtelExporter {
    /// Creates a new OTLP exporter with the given endpoint and service name.
    pub fn new(
        endpoint: &str,
        service_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let reader = PeriodicReader::builder(exporter, Tokio).build();
        let resource = opentelemetry_sdk::Resource::new(vec![opentelemetry::KeyValue::new(
            "service.name",
            service_name.to_string(),
        )]);
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();
        let meter = provider.meter("wsblast");

        let handshake_histogram = meter
            .u64_histogram("wsblast.handshake.latency")
            .with_description("WebSocket handshake latency in microseconds")
            .with_unit("us")
            .build();

        let rtt_histogram = meter
            .u64_histogram("wsblast.message.rtt")
            .with_description("WebSocket message round-trip latency in microseconds")
            .with_unit("us")
            .build();

        let messages_sent_counter = meter
            .u64_counter("wsblast.messages.sent")
            .with_description("Total messages sent")
            .build();

        let messages_received_counter = meter
            .u64_counter("wsblast.messages.received")
            .with_description("Total messages received")
            .build();

        let bytes_sent_counter = meter
            .u64_counter("wsblast.bytes.sent")
            .with_description("Total bytes sent")
            .build();

        let bytes_received_counter = meter
            .u64_counter("wsblast.bytes.received")
            .with_description("Total bytes received")
            .build();

        let active_connections_gauge = meter
            .i64_gauge("wsblast.connections.active")
            .with_description("Currently active WebSocket connections")
            .build();

        let connections_established_counter = meter
            .u64_counter("wsblast.connections.established")
            .with_description("Total connections established")
            .build();

        let connections_failed_counter = meter
            .u64_counter("wsblast.connections.failed")
            .with_description("Total connections failed")
            .build();

        let total_errors_counter = meter
            .u64_counter("wsblast.errors.total")
            .with_description("Total errors encountered")
            .build();

        Ok(Self {
            provider,
            _meter: meter,
            handshake_histogram,
            rtt_histogram,
            messages_sent_counter,
            messages_received_counter,
            bytes_sent_counter,
            bytes_received_counter,
            active_connections_gauge,
            connections_established_counter,
            connections_failed_counter,
            total_errors_counter,
            last_messages_sent: AtomicU64::new(0),
            last_messages_received: AtomicU64::new(0),
            last_bytes_sent: AtomicU64::new(0),
            last_bytes_received: AtomicU64::new(0),
            last_connections_established: AtomicU64::new(0),
            last_connections_failed: AtomicU64::new(0),
            last_total_errors: AtomicU64::new(0),
        })
    }

    /// Records the latest live counters as deltas so repeated scrapes stay accurate.
    pub fn refresh_live(&self, snapshot: &LiveSnapshot) {
        let attrs = &[];

        self.active_connections_gauge
            .record(snapshot.active_connections as i64, attrs);

        self.messages_sent_counter.add(
            counter_delta(&self.last_messages_sent, snapshot.messages_sent),
            attrs,
        );
        self.messages_received_counter.add(
            counter_delta(&self.last_messages_received, snapshot.messages_received),
            attrs,
        );
        self.bytes_sent_counter.add(
            counter_delta(&self.last_bytes_sent, snapshot.bytes_sent),
            attrs,
        );
        self.bytes_received_counter.add(
            counter_delta(&self.last_bytes_received, snapshot.bytes_received),
            attrs,
        );
        self.connections_established_counter.add(
            counter_delta(
                &self.last_connections_established,
                snapshot.connections_established,
            ),
            attrs,
        );
        self.connections_failed_counter.add(
            counter_delta(&self.last_connections_failed, snapshot.connections_failed),
            attrs,
        );
        self.total_errors_counter.add(
            counter_delta(&self.last_total_errors, snapshot.total_errors),
            attrs,
        );
    }

    /// Records final latency histograms.
    pub fn export_final(&self, metrics: &AggregatedMetrics) {
        let attrs = &[];

        if metrics.handshake_latency.count > 0 {
            self.handshake_histogram
                .record(metrics.handshake_latency.mean_us as u64, attrs);
        }
        if metrics.message_rtt.count > 0 {
            self.rtt_histogram
                .record(metrics.message_rtt.mean_us as u64, attrs);
        }
    }

    /// Shuts down the exporter, flushing any pending metrics.
    pub fn shutdown(&self) -> Result<(), opentelemetry_sdk::metrics::MetricError> {
        self.provider.shutdown()
    }
}

/// Prometheus metrics exporter for scraping via /metrics endpoint.
#[cfg(feature = "prometheus")]
pub struct PrometheusExporter {
    handshake_histogram: PromHistogram,
    rtt_histogram: PromHistogram,
    messages_sent: PromCounter,
    messages_received: PromCounter,
    bytes_sent: PromCounter,
    bytes_received: PromCounter,
    active_connections: PromGauge,
    connections_established: PromCounter,
    connections_failed: PromCounter,
    total_errors: PromCounter,
    throughput_msg: PromGauge,
    throughput_bytes: PromGauge,
    error_rate: PromGauge,
    last_messages_sent: AtomicU64,
    last_messages_received: AtomicU64,
    last_bytes_sent: AtomicU64,
    last_bytes_received: AtomicU64,
    last_connections_established: AtomicU64,
    last_connections_failed: AtomicU64,
    last_total_errors: AtomicU64,
}

#[cfg(feature = "prometheus")]
impl PrometheusExporter {
    /// Creates a new Prometheus exporter with default metrics.
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let handshake_histogram = PromHistogram::new(exponential_buckets(100.0, 2.0, 20));
        let rtt_histogram = PromHistogram::new(exponential_buckets(100.0, 2.0, 20));

        let messages_sent = PromCounter::default();
        let messages_received = PromCounter::default();
        let bytes_sent = PromCounter::default();
        let bytes_received = PromCounter::default();
        let active_connections = PromGauge::default();
        let connections_established = PromCounter::default();
        let connections_failed = PromCounter::default();
        let total_errors = PromCounter::default();
        let throughput_msg = PromGauge::default();
        let throughput_bytes = PromGauge::default();
        let error_rate = PromGauge::default();

        Ok(Self {
            handshake_histogram,
            rtt_histogram,
            messages_sent,
            messages_received,
            bytes_sent,
            bytes_received,
            active_connections,
            connections_established,
            connections_failed,
            total_errors,
            throughput_msg,
            throughput_bytes,
            error_rate,
            last_messages_sent: AtomicU64::new(0),
            last_messages_received: AtomicU64::new(0),
            last_bytes_sent: AtomicU64::new(0),
            last_bytes_received: AtomicU64::new(0),
            last_connections_established: AtomicU64::new(0),
            last_connections_failed: AtomicU64::new(0),
            last_total_errors: AtomicU64::new(0),
        })
    }

    /// Creates a registry with all metrics registered for HTTP scraping.
    pub fn build_registry(&self) -> Registry {
        let mut registry = Registry::default();

        registry.register(
            "wsblast_handshake_latency_us",
            "WebSocket handshake latency in microseconds",
            self.handshake_histogram.clone(),
        );
        registry.register(
            "wsblast_message_rtt_us",
            "WebSocket message round-trip latency in microseconds",
            self.rtt_histogram.clone(),
        );
        registry.register(
            "wsblast_messages_sent",
            "Total messages sent",
            self.messages_sent.clone(),
        );
        registry.register(
            "wsblast_messages_received",
            "Total messages received",
            self.messages_received.clone(),
        );
        registry.register(
            "wsblast_bytes_sent",
            "Total bytes sent",
            self.bytes_sent.clone(),
        );
        registry.register(
            "wsblast_bytes_received",
            "Total bytes received",
            self.bytes_received.clone(),
        );
        registry.register(
            "wsblast_connections_active",
            "Currently active WebSocket connections",
            self.active_connections.clone(),
        );
        registry.register(
            "wsblast_connections_established",
            "Total connections established",
            self.connections_established.clone(),
        );
        registry.register(
            "wsblast_connections_failed",
            "Total connections failed",
            self.connections_failed.clone(),
        );
        registry.register(
            "wsblast_errors",
            "Total errors encountered",
            self.total_errors.clone(),
        );
        registry.register(
            "wsblast_throughput_msg_per_sec",
            "Message throughput per second",
            self.throughput_msg.clone(),
        );
        registry.register(
            "wsblast_throughput_bytes_per_sec",
            "Byte throughput per second",
            self.throughput_bytes.clone(),
        );
        registry.register(
            "wsblast_error_rate_percent",
            "Error rate as percentage (0-100)",
            self.error_rate.clone(),
        );

        registry
    }

    /// Updates live counters with deltas so repeated scrapes do not double count.
    pub fn refresh_live(&self, snapshot: &LiveSnapshot) {
        self.active_connections
            .set(snapshot.active_connections as i64);

        self.messages_sent.inc_by(counter_delta(
            &self.last_messages_sent,
            snapshot.messages_sent,
        ));
        self.messages_received.inc_by(counter_delta(
            &self.last_messages_received,
            snapshot.messages_received,
        ));
        self.bytes_sent
            .inc_by(counter_delta(&self.last_bytes_sent, snapshot.bytes_sent));
        self.bytes_received.inc_by(counter_delta(
            &self.last_bytes_received,
            snapshot.bytes_received,
        ));

        self.connections_established.inc_by(counter_delta(
            &self.last_connections_established,
            snapshot.connections_established,
        ));
        self.connections_failed.inc_by(counter_delta(
            &self.last_connections_failed,
            snapshot.connections_failed,
        ));
        self.total_errors.inc_by(counter_delta(
            &self.last_total_errors,
            snapshot.total_errors,
        ));
    }

    /// Records final throughput, error rate, and latency histograms.
    pub fn export_final(&self, metrics: &AggregatedMetrics) {
        self.active_connections.set(0);
        self.throughput_msg
            .set(metrics.throughput_msg_per_sec as i64);
        self.throughput_bytes
            .set(metrics.throughput_bytes_per_sec as i64);
        self.error_rate.set((metrics.error_rate * 100.0) as i64);

        if metrics.handshake_latency.count > 0 {
            self.handshake_histogram
                .observe(metrics.handshake_latency.mean_us);
        }
        if metrics.message_rtt.count > 0 {
            self.rtt_histogram.observe(metrics.message_rtt.mean_us);
        }
    }
}

#[cfg(all(test, feature = "prometheus"))]
mod tests {
    use super::PrometheusExporter;

    #[test]
    fn prometheus_counters_use_single_total_suffix() {
        let exporter = PrometheusExporter::new().expect("prometheus exporter");
        let registry = exporter.build_registry();
        let mut output = String::new();
        prometheus_client::encoding::text::encode(&mut output, &registry).expect("encode metrics");

        assert!(output.contains("wsblast_messages_sent_total 0"));
        assert!(output.contains("wsblast_connections_failed_total 0"));
        assert!(output.contains("wsblast_errors_total 0"));
        assert!(!output.contains("_total_total"));
    }
}
