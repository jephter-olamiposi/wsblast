//! Live export of a run's metrics while it is in progress.
//!
//! [`ExportSource`] reads the run's live counters and the latency of the most
//! recently completed time-series interval into an [`ExportSnapshot`], and
//! [`COUNTERS`] and the other [`MetricInfo`] constants name and describe every
//! exported value. Both exporters render from these, so the Prometheus
//! endpoint and the OTLP exporter cannot disagree on what a metric means.
//! METHODOLOGY.md section 6.4 defines each value.
//!
//! Nothing here runs on the measurement path: workers keep updating the same
//! relaxed counters and interval histograms they always do, and an export
//! reads them.

#[cfg(feature = "otel")]
pub(crate) mod otel;
#[cfg(feature = "prometheus")]
pub(crate) mod prometheus;

use crate::metrics::{LiveMetrics, LiveSnapshot};
use crate::timeseries::IntervalStore;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Name, unit and description of one exported metric.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetricInfo {
    /// Prometheus metric name, with the `_total` suffix for counters.
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    pub(crate) prometheus: &'static str,
    /// OpenTelemetry instrument name.
    #[cfg_attr(not(feature = "otel"), allow(dead_code))]
    pub(crate) otel: &'static str,
    /// UCUM unit of the OpenTelemetry instrument.
    #[cfg_attr(not(feature = "otel"), allow(dead_code))]
    pub(crate) unit: &'static str,
    /// HELP text and instrument description.
    pub(crate) help: &'static str,
}

/// A monotonic counter read from the live counters.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CounterDef {
    pub(crate) info: MetricInfo,
    pub(crate) read: fn(&LiveSnapshot) -> u64,
}

/// Every exported counter, in exposition order.
pub(crate) const COUNTERS: [CounterDef; 8] = [
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_messages_sent_total",
            otel: "wsblast.messages.sent",
            unit: "{message}",
            help: "Data frames written successfully.",
        },
        read: |s| s.messages_sent,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_messages_received_total",
            otel: "wsblast.messages.received",
            unit: "{message}",
            help: "Data frames counted as received.",
        },
        read: |s| s.messages_received,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_bytes_sent_total",
            otel: "wsblast.bytes.sent",
            unit: "By",
            help: "Payload bytes of the data frames sent.",
        },
        read: |s| s.bytes_sent,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_bytes_received_total",
            otel: "wsblast.bytes.received",
            unit: "By",
            help: "Payload bytes of the data frames received.",
        },
        read: |s| s.bytes_received,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_errors_total",
            otel: "wsblast.errors",
            unit: "{error}",
            help: "Errors recorded in every category.",
        },
        read: |s| s.total_errors,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_connections_established_total",
            otel: "wsblast.connections.established",
            unit: "{connection}",
            help: "Connection slots that completed their first handshake.",
        },
        read: |s| s.connections_established,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_connections_failed_total",
            otel: "wsblast.connections.failed",
            unit: "{attempt}",
            help: "Failed handshake attempts, including failed reconnect attempts.",
        },
        read: |s| s.connections_failed,
    },
    CounterDef {
        info: MetricInfo {
            prometheus: "wsblast_reconnects_total",
            otel: "wsblast.reconnects",
            unit: "{connection}",
            help: "Handshakes that re-established a connection slot after it lost its connection.",
        },
        read: |s| s.reconnects,
    },
];

/// Gauge: connections open now.
pub(crate) const ACTIVE_CONNECTIONS: MetricInfo = MetricInfo {
    prometheus: "wsblast_active_connections",
    otel: "wsblast.connections.active",
    unit: "{connection}",
    help: "WebSocket connections currently open.",
};

/// Gauge: time since the run's clock started.
pub(crate) const RUN_ELAPSED: MetricInfo = MetricInfo {
    prometheus: "wsblast_run_elapsed_seconds",
    otel: "wsblast.run.elapsed",
    unit: "s",
    help: "Seconds since the run's clock started; frozen when the run ends.",
};

/// Gauge family: latency percentiles of the last completed interval.
pub(crate) const LATENCY: MetricInfo = MetricInfo {
    prometheus: "wsblast_latency_seconds",
    otel: "wsblast.latency",
    unit: "s",
    help: "Latency percentiles of the most recently completed time-series interval.",
};

/// Gauge: samples behind [`LATENCY`].
pub(crate) const LATENCY_SAMPLES: MetricInfo = MetricInfo {
    prometheus: "wsblast_latency_samples",
    otel: "wsblast.latency.samples",
    unit: "{sample}",
    help: "Latency samples recorded in the most recently completed time-series interval.",
};

/// Everything an export publishes, read at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ExportSnapshot {
    /// The live counters, summed over every shard.
    pub(crate) counters: LiveSnapshot,
    /// Time since the run's clock started, or the run's length once it ended.
    pub(crate) elapsed: Duration,
    /// Latency of the most recently completed interval of the current run or
    /// scenario phase; `None` before the first interval completes.
    pub(crate) latency: Option<IntervalExport>,
}

/// Latency of one completed time-series interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IntervalExport {
    /// `rtt` or `response_time`, as in the time series' `latency_basis`.
    pub(crate) basis: &'static str,
    /// Number of samples; the percentiles are 0 when it is 0.
    pub(crate) count: u64,
    pub(crate) p50_us: u64,
    pub(crate) p90_us: u64,
    pub(crate) p99_us: u64,
}

impl IntervalExport {
    /// The exported percentiles as (quantile label, seconds), or nothing when
    /// the interval has no samples.
    pub(crate) fn quantiles(&self) -> Vec<(&'static str, f64)> {
        if self.count == 0 {
            return Vec::new();
        }
        let secs = |us: u64| us as f64 / 1_000_000.0;
        vec![
            ("0.5", secs(self.p50_us)),
            ("0.9", secs(self.p90_us)),
            ("0.99", secs(self.p99_us)),
        ]
    }
}

/// The time series of the run or scenario phase in progress.
#[derive(Debug)]
struct Phase {
    store: Arc<IntervalStore>,
    basis: &'static str,
    /// The phase's `elapsed`, once it has ended.
    ended: Option<Duration>,
}

#[derive(Debug, Default)]
struct State {
    run_start: Option<Instant>,
    finished: Option<Duration>,
    phase: Option<Phase>,
    /// Set while a distributed coordinator runs a phase: the latency it
    /// merged from its agents, which replaces the local time series.
    #[cfg(feature = "distributed")]
    remote: Option<Option<IntervalExport>>,
}

/// What a run publishes to its exporters: the shared live counters plus the
/// time series of the phase in progress, which the runner registers.
#[derive(Debug)]
pub(crate) struct ExportSource {
    live: Arc<LiveMetrics>,
    state: Mutex<State>,
}

impl ExportSource {
    /// A source over `live`, the registry every phase of the run shares.
    pub(crate) fn new(live: Arc<LiveMetrics>) -> Arc<Self> {
        Arc::new(Self {
            live,
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers the time series of a run or phase whose clock just started.
    /// The first call also starts the clock of `wsblast_run_elapsed_seconds`.
    pub(crate) fn begin_phase(&self, store: Arc<IntervalStore>, basis: &'static str) {
        let mut state = self.state();
        state.run_start.get_or_insert(store.start());
        #[cfg(feature = "distributed")]
        {
            state.remote = None;
        }
        state.phase = Some(Phase {
            store,
            basis,
            ended: None,
        });
    }

    /// Marks the current phase as ended after `elapsed`, once every worker
    /// has handed over its last interval.
    pub(crate) fn end_phase(&self, elapsed: Duration) {
        if let Some(phase) = self.state().phase.as_mut() {
            phase.ended = Some(elapsed);
        }
    }

    /// Freezes the elapsed time once the whole run has ended.
    pub(crate) fn finish(&self) {
        let mut state = self.state();
        if state.finished.is_none() {
            state.finished = Some(state.run_start.map_or(Duration::ZERO, |s| s.elapsed()));
        }
    }

    /// The live counters, summed over every shard. Takes no lock.
    pub(crate) fn counters(&self) -> LiveSnapshot {
        self.live.snapshot()
    }

    /// Time since the run's clock started, frozen by [`ExportSource::finish`];
    /// zero before the run starts.
    pub(crate) fn elapsed(&self) -> Duration {
        let state = self.state();
        state
            .finished
            .or_else(|| state.run_start.map(|start| start.elapsed()))
            .unwrap_or_default()
    }

    /// Starts a phase that runs on distributed agents at `start`: the first
    /// call starts the clock of `wsblast_run_elapsed_seconds`, and the
    /// latency is what [`ExportSource::publish_latency`] last set, nothing
    /// until then.
    #[cfg(feature = "distributed")]
    pub(crate) fn begin_remote_phase(&self, start: Instant) {
        let mut state = self.state();
        state.run_start.get_or_insert(start);
        state.phase = None;
        state.remote = Some(None);
    }

    /// Publishes the latency a distributed coordinator merged from its
    /// agents' interval histograms.
    #[cfg(feature = "distributed")]
    pub(crate) fn publish_latency(&self, latency: Option<IntervalExport>) {
        self.state().remote = Some(latency);
    }

    /// Latency of the last completed interval of the current run or phase.
    pub(crate) fn latency(&self) -> Option<IntervalExport> {
        let state = self.state();
        #[cfg(feature = "distributed")]
        if let Some(remote) = state.remote {
            return remote;
        }
        let phase = state.phase.as_ref()?;
        let since_start = phase.ended.unwrap_or_else(|| phase.store.start().elapsed());
        let bucket = last_completed(since_start, phase.store.interval())?;
        let latency = phase.store.latency_of(bucket);
        Some(IntervalExport {
            basis: phase.basis,
            count: latency.count,
            p50_us: latency.p50_us,
            p90_us: latency.p90_us,
            p99_us: latency.p99_us,
        })
    }

    /// Reads every exported value.
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    pub(crate) fn snapshot(&self) -> ExportSnapshot {
        ExportSnapshot {
            counters: self.counters(),
            elapsed: self.elapsed(),
            latency: self.latency(),
        }
    }
}

/// Index of the last interval that ended at or before `since_start`.
pub(crate) fn last_completed(since_start: Duration, interval: Duration) -> Option<u64> {
    let index = since_start.as_nanos() / interval.as_nanos().max(1);
    u64::try_from(index).ok()?.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeseries::interval_histogram;

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
    fn snapshot_reads_counters_and_the_latency_of_the_completed_interval() {
        let live = LiveMetrics::new();
        live.shard(0)
            .messages_sent
            .fetch_add(7, std::sync::atomic::Ordering::Relaxed);
        let source = ExportSource::new(Arc::clone(&live));
        // Before the run starts: counters, but no clock and no interval.
        let before = source.snapshot();
        assert_eq!(before.counters.messages_sent, 7);
        assert_eq!((before.elapsed, before.latency), (Duration::ZERO, None));

        let start = Instant::now()
            .checked_sub(Duration::from_millis(1_500))
            .unwrap();
        let store = IntervalStore::new(start, Duration::from_secs(1));
        let mut hist = interval_histogram().unwrap();
        hist.record(2_000).unwrap();
        store.absorb(0, &hist);
        // Samples of the interval in progress are not exported yet.
        store.absorb(1, &hist);
        store.absorb(1, &hist);
        source.begin_phase(Arc::clone(&store), "response_time");

        let snap = source.snapshot();
        assert_eq!(snap.counters.messages_sent, 7);
        assert!(snap.elapsed >= Duration::from_millis(1_500));
        let latency = snap.latency.unwrap();
        assert_eq!((latency.basis, latency.count), ("response_time", 1));
        assert!((1_980..=2_020).contains(&latency.p99_us));
        assert_eq!(latency.quantiles().len(), 3);

        // Once the phase has ended at 2.2 s, interval 1 is complete.
        source.end_phase(Duration::from_millis(2_200));
        assert_eq!(source.snapshot().latency.unwrap().count, 2);

        source.finish();
        let frozen = source.snapshot().elapsed;
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(source.snapshot().elapsed, frozen);
    }

    #[test]
    fn an_interval_without_samples_exports_no_quantiles() {
        let empty = IntervalExport {
            basis: "rtt",
            ..IntervalExport::default()
        };
        assert!(empty.quantiles().is_empty());
    }
}
