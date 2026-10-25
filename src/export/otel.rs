//! Periodic export of the live metrics to an OpenTelemetry collector over
//! OTLP/HTTP with protobuf bodies.
//!
//! Every instrument is observable: the SDK's periodic reader runs on its own
//! thread, calls the callbacks below at each export interval, and sends the
//! result with a blocking HTTP client on that same thread. Workers never wait
//! for an export, and a failed export only prints one warning.

use super::{ACTIVE_CONNECTIONS, COUNTERS, ExportSource, LATENCY, LATENCY_SAMPLES, RUN_ELAPSED};
use crate::error::{ConfigError, Result};
use colored::Colorize;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_otlp::{
    MetricExporter, Protocol, RetryPolicy, WithExportConfig as _, WithHttpConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

/// Longest a single export may take.
const MAX_EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Path of the metrics signal under an OTLP/HTTP base URL.
const METRICS_PATH: &str = "/v1/metrics";

/// What `--otel-*` asked for, validated.
#[derive(Debug, Clone)]
pub(crate) struct OtelSettings {
    /// Full URL export requests are posted to.
    pub(crate) metrics_url: String,
    pub(crate) interval: Duration,
    pub(crate) service_name: String,
    /// Host of the load test's target URL.
    pub(crate) target_host: String,
    /// The `--mode` name, such as `echo`.
    pub(crate) mode: String,
    /// Hold the failure warning until the run ends, for the TUI.
    pub(crate) defer_warning: bool,
}

/// Builds the URL export requests go to from an OTLP/HTTP base URL such as
/// `http://localhost:4318`. A URL that already ends in `/v1/metrics` is used
/// as given.
pub(crate) fn metrics_url(base: &str) -> Result<String> {
    let invalid =
        |reason: String| ConfigError::InvalidOption(format!("--otel-endpoint '{base}': {reason}"));
    let url = Url::parse(base).map_err(|e| invalid(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("expected an http:// or https:// URL".into()).into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("the URL has no host".into()).into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("the URL must not have a query or fragment".into()).into());
    }
    let mut url = url.to_string();
    while url.ends_with('/') {
        url.pop();
    }
    if !url.ends_with(METRICS_PATH) {
        url.push_str(METRICS_PATH);
    }
    Ok(url)
}

/// Remembers the first export failure and reports it once.
#[derive(Debug, Default)]
struct FailureNotice {
    reported: AtomicBool,
    deferred: Mutex<Option<String>>,
}

impl FailureNotice {
    fn record(&self, message: String, defer: bool) {
        if self.reported.swap(true, Ordering::Relaxed) {
            return;
        }
        if defer {
            *self.deferred.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        } else {
            warn(&message);
        }
    }

    fn print_deferred(&self) {
        if let Some(message) = self
            .deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            warn(&message);
        }
    }
}

fn warn(message: &str) {
    eprintln!("{} {message}", "Warning:".bold().yellow());
}

/// Passes every call to the OTLP exporter and turns its first failure into a
/// warning. The error still goes back to the SDK, which drops that batch;
/// the next interval exports cumulative values again.
struct WarningExporter {
    inner: MetricExporter,
    notice: Arc<FailureNotice>,
    url: String,
    defer: bool,
}

impl PushMetricExporter for WarningExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        if let Err(err) = &result {
            self.notice.record(
                format!(
                    "OpenTelemetry export to {} failed: {err}. The run continues; later export failures are not reported.",
                    self.url
                ),
                self.defer,
            );
        }
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

/// A running OTLP export. Call [`OtelExporter::finish`] when the run ends.
pub(crate) struct OtelExporter {
    provider: SdkMeterProvider,
    notice: Arc<FailureNotice>,
}

impl OtelExporter {
    /// Starts exporting `source` every `settings.interval`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] when the HTTP client or the
    /// exporter cannot be built.
    pub(crate) fn start(settings: OtelSettings, source: Arc<ExportSource>) -> Result<Self> {
        let timeout = settings.interval.min(MAX_EXPORT_TIMEOUT);
        let client = blocking_client(timeout)?;
        let exporter = MetricExporter::builder()
            // The counters are cumulative by nature; this also keeps
            // OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE from turning
            // them into deltas.
            .with_temporality(Temporality::Cumulative)
            .with_http()
            .with_http_client(client)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(settings.metrics_url.clone())
            .with_timeout(timeout)
            // A load test exports cumulative values, so the next interval
            // replaces a lost batch; retrying would only delay it.
            .with_retry_policy(RetryPolicy::disabled())
            .build()
            .map_err(|e| ConfigError::InvalidOption(format!("OpenTelemetry exporter: {e}")))?;
        let notice = Arc::new(FailureNotice::default());
        let exporter = WarningExporter {
            inner: exporter,
            notice: Arc::clone(&notice),
            url: settings.metrics_url.clone(),
            defer: settings.defer_warning,
        };
        let reader = PeriodicReader::builder(exporter)
            .with_interval(settings.interval)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(
                Resource::builder()
                    .with_service_name(settings.service_name.clone())
                    .build(),
            )
            .build();
        register_instruments(&provider, &settings, &source);
        Ok(Self { provider, notice })
    }

    /// Exports the final values once, stops the exporter and prints a
    /// warning held back for the TUI.
    pub(crate) async fn finish(self) {
        let Self { provider, notice } = self;
        // Shutdown blocks until the last export is sent or times out, and the
        // blocking HTTP client must not be dropped on a runtime thread.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = provider.shutdown();
            drop(provider);
        })
        .await;
        notice.print_deferred();
    }
}

/// Builds the HTTP client on its own thread: the blocking client starts an
/// internal runtime, which must not happen on a Tokio worker thread.
fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    let build = move || {
        reqwest::blocking::Client::builder()
            .timeout(timeout)
            .tls_backend_preconfigured(crate::connect::rustls_client_config())
            .build()
    };
    std::thread::spawn(build)
        .join()
        .map_err(|_| ConfigError::InvalidOption("OpenTelemetry HTTP client: panicked".into()))?
        .map_err(|e| ConfigError::InvalidOption(format!("OpenTelemetry HTTP client: {e}")).into())
}

/// Registers one observable instrument per exported metric.
///
/// The SDK keeps the callbacks for the provider's lifetime, so the returned
/// instrument handles do not need to be kept.
fn register_instruments(
    provider: &SdkMeterProvider,
    settings: &OtelSettings,
    source: &Arc<ExportSource>,
) {
    let scope = InstrumentationScope::builder("wsblast")
        .with_version(env!("CARGO_PKG_VERSION"))
        .build();
    let meter = provider.meter_with_scope(scope);
    let attrs: Arc<[KeyValue]> = Arc::new([
        KeyValue::new("server.address", settings.target_host.clone()),
        KeyValue::new("wsblast.mode", settings.mode.clone()),
    ]);

    for counter in COUNTERS {
        let source = Arc::clone(source);
        let attrs = Arc::clone(&attrs);
        meter
            .u64_observable_counter(counter.info.otel)
            .with_unit(counter.info.unit)
            .with_description(counter.info.help)
            .with_callback(move |observer| {
                observer.observe((counter.read)(&source.counters()), &attrs);
            })
            .build();
    }

    {
        let source = Arc::clone(source);
        let attrs = Arc::clone(&attrs);
        meter
            .u64_observable_gauge(ACTIVE_CONNECTIONS.otel)
            .with_unit(ACTIVE_CONNECTIONS.unit)
            .with_description(ACTIVE_CONNECTIONS.help)
            .with_callback(move |observer| {
                observer.observe(source.counters().active_connections, &attrs);
            })
            .build();
    }

    {
        let source = Arc::clone(source);
        let attrs = Arc::clone(&attrs);
        meter
            .f64_observable_gauge(RUN_ELAPSED.otel)
            .with_unit(RUN_ELAPSED.unit)
            .with_description(RUN_ELAPSED.help)
            .with_callback(move |observer| {
                observer.observe(source.elapsed().as_secs_f64(), &attrs);
            })
            .build();
    }

    {
        let source = Arc::clone(source);
        let attrs = Arc::clone(&attrs);
        meter
            .f64_observable_gauge(LATENCY.otel)
            .with_unit(LATENCY.unit)
            .with_description(LATENCY.help)
            .with_callback(move |observer| {
                let Some(latency) = source.latency() else {
                    return;
                };
                for (quantile, secs) in latency.quantiles() {
                    let mut point = attrs.to_vec();
                    point.push(KeyValue::new("basis", latency.basis));
                    point.push(KeyValue::new("quantile", quantile));
                    observer.observe(secs, &point);
                }
            })
            .build();
    }

    {
        let source = Arc::clone(source);
        meter
            .u64_observable_gauge(LATENCY_SAMPLES.otel)
            .with_unit(LATENCY_SAMPLES.unit)
            .with_description(LATENCY_SAMPLES.help)
            .with_callback(move |observer| {
                let Some(latency) = source.latency() else {
                    return;
                };
                let mut point = attrs.to_vec();
                point.push(KeyValue::new("basis", latency.basis));
                observer.observe(latency.count, &point);
            })
            .build();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metrics_path_is_appended_to_the_base_url() {
        let url = |base| metrics_url(base).unwrap();
        assert_eq!(
            url("http://localhost:4318"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            url("http://localhost:4318/"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(
            url("https://otel.example.com/otlp"),
            "https://otel.example.com/otlp/v1/metrics"
        );
        assert_eq!(
            url("http://127.0.0.1:4318/v1/metrics"),
            "http://127.0.0.1:4318/v1/metrics"
        );
        for bad in [
            "localhost:4318",
            "ws://localhost:4318",
            "http://",
            "http://h/?q=1",
        ] {
            assert!(metrics_url(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn only_the_first_failure_is_kept() {
        let notice = FailureNotice::default();
        notice.record("first".into(), true);
        notice.record("second".into(), true);
        assert_eq!(notice.deferred.lock().unwrap().as_deref(), Some("first"));
    }
}
