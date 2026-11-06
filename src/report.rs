//! SLO evaluation, report rendering (terminal text, JSON schema 1.0,
//! Markdown), and the baseline comparison behind `--compare` and its
//! rendering (text, JSON, Markdown; HTML is in `html.rs`).

use crate::cli::OutputFormat;
use crate::config::{LoadTestConfig, SloThresholds};
use crate::metrics::{
    AggregatedMetrics, CaptureSummary, ChaosSummary, DistributedSummary, LatencyStats,
};
#[cfg(feature = "compare")]
use crate::series_compare::{CompareWindow, SERIES_PREFIX, SeriesSummary};
use colored::Colorize;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Color, Row, Table};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
#[cfg(feature = "compare")]
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

/// Outcome of [`evaluate_slos`]: one check per configured threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SloEvaluation {
    /// True when every check passed, including when no threshold was set.
    pub passed: bool,
    /// Checks in a fixed order: p50, p95, p99, p99.9, error rate, throughput.
    pub checks: Vec<SloCheck>,
}

/// One evaluated SLO threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SloCheck {
    /// Human-readable name, for example `Latency p99`.
    pub name: String,
    /// The threshold, formatted for display (`<= 25ms`).
    pub target: String,
    /// The measured value, formatted for display, or `no RTT samples`.
    pub actual: String,
    /// Whether the measured value satisfies the threshold.
    pub passed: bool,
}

/// Endpoint one scenario phase ran against, listed in scenario reports.
#[cfg_attr(docsrs, doc(cfg(feature = "scenario")))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PhaseTarget {
    /// Phase name.
    pub phase: String,
    /// Endpoint the phase connected to.
    pub target_url: url::Url,
}

/// One executed scenario phase as the reports show it: its target, size,
/// own metrics and own SLO result. The JSON report lists one per phase under
/// `phases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PhaseReport {
    /// Phase name.
    pub(crate) name: String,
    /// Endpoint the phase connected to.
    pub(crate) target_url: url::Url,
    /// Configured connection count of the phase; for a staged phase, the
    /// highest stage target.
    pub(crate) connections: usize,
    /// The phase's load stages; empty for a constant connection count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) stages: Vec<StageReport>,
    /// Metrics merged from this phase's workers only.
    pub(crate) metrics: AggregatedMetrics,
    /// The phase's `slo:` thresholds checked on `metrics`.
    pub(crate) slo: SloEvaluation,
}

#[cfg(feature = "scenario")]
impl From<&crate::runner::PhaseResult> for PhaseReport {
    fn from(phase: &crate::runner::PhaseResult) -> Self {
        Self {
            name: phase.name.clone(),
            target_url: phase.target_url.clone(),
            connections: phase.connections,
            stages: phase
                .stages
                .iter()
                .map(|stage| StageReport {
                    duration: stage.duration,
                    target: stage.target,
                })
                .collect(),
            metrics: phase.metrics.clone(),
            slo: phase.slo.clone(),
        }
    }
}

/// One load stage of a scenario phase, as the JSON report writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StageReport {
    /// How long the stage lasts.
    pub(crate) duration: Duration,
    /// Connections open at the end of the stage.
    pub(crate) target: usize,
}

/// Describes load stages as `DURATION:TARGET` pairs, the syntax of
/// `--stages`.
#[cfg(feature = "scenario")]
pub(crate) fn stage_list_label(stages: &[crate::config::Stage]) -> String {
    stages
        .iter()
        .map(|stage| format!("{:?}:{}", stage.duration, stage.target))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Describes recorded load stages as `DURATION:TARGET` pairs, the syntax of
/// `--stages`.
fn stage_reports_label(stages: &[StageReport]) -> String {
    stages
        .iter()
        .map(|stage| format!("{:?}:{}", stage.duration, stage.target))
        .collect::<Vec<_>>()
        .join(", ")
}

impl PhaseReport {
    /// What the phase ran: its stages, or its connection count.
    pub(crate) fn load_label(&self) -> String {
        if !self.stages.is_empty() {
            return stage_reports_label(&self.stages);
        }
        match self.connections {
            1 => "1 connection".to_string(),
            n => format!("{n} connections"),
        }
    }

    /// The p99 the phase's latency SLOs gate on, with its basis.
    pub(crate) fn gating_p99_label(&self) -> String {
        let (stats, basis) = if self.metrics.response_time.count > 0 {
            (&self.metrics.response_time, "response time")
        } else {
            (&self.metrics.message_rtt, "RTT")
        };
        if stats.count == 0 {
            "-".to_string()
        } else {
            format!("{} ({basis})", format_us(stats.p99_us))
        }
    }

    /// `PASSED`, `FAILED`, or `-` when the phase sets no SLO.
    pub(crate) fn slo_label(&self) -> &'static str {
        match (self.slo.checks.is_empty(), self.slo.passed) {
            (true, _) => "-",
            (false, true) => "PASSED",
            (false, false) => "FAILED",
        }
    }
}

/// Whether the text and Markdown reports show the per-phase table: only when
/// a phase has its own SLO or load stages, so the reports of scenarios with
/// neither are unchanged.
fn shows_phase_table(phases: &[PhaseReport]) -> bool {
    phases
        .iter()
        .any(|phase| !phase.slo.checks.is_empty() || !phase.stages.is_empty())
}

/// Combines the evaluation of the whole scenario with each phase's own,
/// whose check names are prefixed with the phase name (`spike: Latency
/// p99`). It passes only when every check passed.
#[cfg(feature = "scenario")]
pub(crate) fn scenario_slo_evaluation<'a>(
    overall: SloEvaluation,
    phases: impl IntoIterator<Item = (&'a str, &'a SloEvaluation)>,
) -> SloEvaluation {
    let mut combined = overall;
    for (name, evaluation) in phases {
        combined.passed &= evaluation.passed;
        combined
            .checks
            .extend(evaluation.checks.iter().map(|check| SloCheck {
                name: format!("{name}: {}", check.name),
                ..check.clone()
            }));
    }
    combined
}

/// Baseline comparison for regression detection.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BaselineComparison {
    pub(crate) baseline: AggregatedMetrics,
    pub(crate) current: AggregatedMetrics,
    pub(crate) regressions: Vec<Regression>,
    pub(crate) improvements: Vec<Improvement>,
    /// The `--compare-window` the time-series metrics were computed over.
    pub(crate) window: Option<CompareWindow>,
    /// Whether both reports had a time series, so `series.*` metrics were
    /// compared.
    pub(crate) series_compared: bool,
    /// Every metric that was scored, changed beyond the threshold or not,
    /// in metric-name order. Not part of the comparison JSON.
    #[serde(skip)]
    pub(crate) compared: Vec<ComparedMetric>,
}

/// One scored metric of a comparison.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComparedMetric {
    pub(crate) metric: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) pct_change: f64,
    pub(crate) higher_is_better: bool,
    pub(crate) outcome: ChangeOutcome,
}

/// How a compared metric moved relative to the threshold.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeOutcome {
    Regression,
    Improvement,
    WithinThreshold,
}

/// `pct_change` is the signed relative change from baseline to current, in percent.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Regression {
    pub(crate) metric: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) pct_change: f64,
}

/// `pct_change` is the signed relative change from baseline to current, in
/// percent: positive for a throughput gain, negative for a latency drop.
#[cfg(feature = "compare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Improvement {
    pub(crate) metric: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) pct_change: f64,
}

/// Report payloads are either the schema envelope (`{ "metrics": ... }`) or a
/// bare aggregated-metrics object produced by earlier versions.
#[cfg(feature = "compare")]
#[derive(Deserialize)]
#[serde(untagged)]
enum ReportDocument {
    Envelope { metrics: AggregatedMetrics },
    Bare(AggregatedMetrics),
}

/// Extracts aggregated metrics from a schema v1.0 report or a bare metrics object.
///
/// # Errors
///
/// Returns a [`serde_json::Error`] when the document matches neither shape.
#[cfg(feature = "compare")]
pub(crate) fn parse_report_metrics(json: &str) -> serde_json::Result<AggregatedMetrics> {
    match serde_json::from_str::<ReportDocument>(json)? {
        ReportDocument::Envelope { metrics } | ReportDocument::Bare(metrics) => Ok(metrics),
    }
}

#[cfg(feature = "compare")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
    Neutral,
}

/// Whether a higher value of a compared metric is better, for a metric name
/// read from a comparison JSON. Neutral metrics are never in one.
#[cfg(feature = "compare")]
pub(crate) fn higher_is_better(metric: &str) -> bool {
    metric_direction(metric) == MetricDirection::HigherIsBetter
}

/// Classifies a metric by the direction that constitutes an improvement.
#[cfg(feature = "compare")]
fn metric_direction(metric: &str) -> MetricDirection {
    match metric {
        // Input sizing, not an outcome: comparing it would flag intentional reconfiguration.
        "total_connections_attempted" => MetricDirection::Neutral,
        "total_connections_failed" | "error_rate" => MetricDirection::LowerIsBetter,
        _ if metric.starts_with("errors.") => MetricDirection::LowerIsBetter,
        _ if metric.starts_with(SERIES_PREFIX) => {
            if crate::series_compare::higher_is_better(metric) {
                MetricDirection::HigherIsBetter
            } else {
                MetricDirection::LowerIsBetter
            }
        }
        _ if metric.starts_with("handshake_latency.")
            || metric.starts_with("message_rtt.")
            || metric.starts_with("response_time.") =>
        {
            MetricDirection::LowerIsBetter
        }
        _ => MetricDirection::HigherIsBetter,
    }
}

/// [`compare_metrics_in`] over the whole run.
#[cfg(all(feature = "compare", test))]
pub(crate) fn compare_metrics(
    baseline: &AggregatedMetrics,
    current: &AggregatedMetrics,
    threshold: f64,
) -> BaselineComparison {
    compare_metrics_in(baseline, current, threshold, None)
}

/// Compares two metric snapshots and detects regressions/improvements.
///
/// Each metric is scored by its own direction: throughput and delivery counts
/// higher is better, while errors and latency percentiles lower is better. A
/// metric that appears from a zero baseline is reported as a full 100% move so
/// new error categories cannot be silently skipped.
///
/// When both reports have a time series, the `series.*` metrics of
/// [`SeriesSummary`] are compared too, computed over the points inside
/// `window` (every point when it is `None`). Whole-run metrics ignore the
/// window.
#[cfg(feature = "compare")]
pub(crate) fn compare_metrics_in(
    baseline: &AggregatedMetrics,
    current: &AggregatedMetrics,
    threshold: f64,
    window: Option<&CompareWindow>,
) -> BaselineComparison {
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    let mut compared = Vec::new();

    let mut baseline_map = metrics_to_map(baseline);
    let mut current_map = metrics_to_map(current);
    SeriesSummary::of(&baseline.time_series, window).insert_into(&mut baseline_map);
    SeriesSummary::of(&current.time_series, window).insert_into(&mut current_map);
    let has_series =
        |map: &BTreeMap<String, f64>| map.keys().any(|name| name.starts_with(SERIES_PREFIX));
    let series_compared = has_series(&baseline_map) && has_series(&current_map);
    let metric_names: std::collections::BTreeSet<&String> =
        baseline_map.keys().chain(current_map.keys()).collect();

    for metric_name in metric_names {
        let direction = metric_direction(metric_name);
        if direction == MetricDirection::Neutral {
            continue;
        }

        // Response time exists only for paced runs and in reports from 0.2.1
        // on, and the time series only in reports from 0.2.1 on; comparing
        // either against a report without it would read as a regression
        // from zero. A time-series metric can also be undefined for one
        // report, such as the throughput CV of a single interval.
        if (metric_name.starts_with("response_time.") || metric_name.starts_with(SERIES_PREFIX))
            && !(baseline_map.contains_key(metric_name) && current_map.contains_key(metric_name))
        {
            continue;
        }

        let baseline_val = baseline_map.get(metric_name).copied().unwrap_or(0.0);
        let current_val = current_map.get(metric_name).copied().unwrap_or(0.0);
        if baseline_val == 0.0 && current_val == 0.0 {
            continue;
        }

        // A non-zero value against a zero baseline has no finite relative change;
        // use a full move so the direction still classifies it.
        let pct_change = if baseline_val == 0.0 {
            100.0
        } else {
            (current_val - baseline_val) / baseline_val * 100.0
        };

        let (regressed, improved) = match direction {
            MetricDirection::HigherIsBetter => (pct_change < -threshold, pct_change > threshold),
            MetricDirection::LowerIsBetter => (pct_change > threshold, pct_change < -threshold),
            MetricDirection::Neutral => (false, false),
        };

        compared.push(ComparedMetric {
            metric: metric_name.clone(),
            baseline: baseline_val,
            current: current_val,
            pct_change,
            higher_is_better: direction == MetricDirection::HigherIsBetter,
            outcome: if regressed {
                ChangeOutcome::Regression
            } else if improved {
                ChangeOutcome::Improvement
            } else {
                ChangeOutcome::WithinThreshold
            },
        });

        if regressed {
            regressions.push(Regression {
                metric: metric_name.clone(),
                baseline: baseline_val,
                current: current_val,
                pct_change,
            });
        } else if improved {
            improvements.push(Improvement {
                metric: metric_name.clone(),
                baseline: baseline_val,
                current: current_val,
                pct_change,
            });
        }
    }

    BaselineComparison {
        baseline: baseline.clone(),
        current: current.clone(),
        regressions,
        improvements,
        window: window.copied(),
        series_compared,
        compared,
    }
}

#[cfg(feature = "compare")]
fn metrics_to_map(metrics: &AggregatedMetrics) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();

    map.insert(
        "total_connections_attempted".into(),
        metrics.total_connections_attempted as f64,
    );
    map.insert(
        "total_connections_established".into(),
        metrics.total_connections_established as f64,
    );
    map.insert(
        "total_connections_failed".into(),
        metrics.total_connections_failed as f64,
    );
    map.insert(
        "total_messages_sent".into(),
        metrics.total_messages_sent as f64,
    );
    map.insert(
        "total_messages_recv".into(),
        metrics.total_messages_recv as f64,
    );
    map.insert("total_bytes_sent".into(), metrics.total_bytes_sent as f64);
    map.insert("total_bytes_recv".into(), metrics.total_bytes_recv as f64);
    map.insert(
        "throughput_msg_per_sec".into(),
        metrics.throughput_msg_per_sec,
    );
    map.insert(
        "throughput_bytes_per_sec".into(),
        metrics.throughput_bytes_per_sec,
    );
    map.insert("error_rate".into(), metrics.error_rate);
    for (category, count) in &metrics.error_breakdown {
        let name = serde_json::to_string(category)
            .unwrap_or_else(|_| "\"other\"".to_string())
            .trim_matches('"')
            .to_string();
        map.insert(format!("errors.{name}"), *count as f64);
    }
    map.insert(
        "handshake_latency.p50_us".into(),
        metrics.handshake_latency.p50_us as f64,
    );
    map.insert(
        "handshake_latency.p95_us".into(),
        metrics.handshake_latency.p95_us as f64,
    );
    map.insert(
        "handshake_latency.p99_us".into(),
        metrics.handshake_latency.p99_us as f64,
    );
    map.insert(
        "handshake_latency.p999_us".into(),
        metrics.handshake_latency.p999_us as f64,
    );
    map.insert(
        "message_rtt.p50_us".into(),
        metrics.message_rtt.p50_us as f64,
    );
    map.insert(
        "message_rtt.p95_us".into(),
        metrics.message_rtt.p95_us as f64,
    );
    map.insert(
        "message_rtt.p99_us".into(),
        metrics.message_rtt.p99_us as f64,
    );
    map.insert(
        "message_rtt.p999_us".into(),
        metrics.message_rtt.p999_us as f64,
    );
    if metrics.response_time.count > 0 {
        let rt = &metrics.response_time;
        map.insert("response_time.p50_us".into(), rt.p50_us as f64);
        map.insert("response_time.p95_us".into(), rt.p95_us as f64);
        map.insert("response_time.p99_us".into(), rt.p99_us as f64);
        map.insert("response_time.p999_us".into(), rt.p999_us as f64);
    }

    map
}

/// What the comparison renderers read: a comparison computed from two
/// reports, or one read back from a comparison JSON.
#[cfg(feature = "compare")]
pub(crate) struct ComparisonView<'a> {
    pub(crate) threshold: f64,
    pub(crate) window: Option<CompareWindow>,
    pub(crate) regressions: &'a [Regression],
    pub(crate) improvements: &'a [Improvement],
    pub(crate) source: ComparisonSource<'a>,
}

/// Where a [`ComparisonView`] comes from.
#[cfg(feature = "compare")]
pub(crate) enum ComparisonSource<'a> {
    /// Computed now from two reports.
    Reports {
        /// The baseline and current report files, as given.
        files: Option<(String, String)>,
        baseline: &'a AggregatedMetrics,
        current: &'a AggregatedMetrics,
        series_compared: bool,
        compared: &'a [ComparedMetric],
    },
    /// Read from a comparison JSON, which holds only the changes beyond the
    /// threshold and no time series.
    Saved {
        schema_version: &'a str,
        /// What the timestamp is and when, for the header.
        timestamp: Option<(&'static str, String)>,
    },
}

#[cfg(feature = "compare")]
impl BaselineComparison {
    /// The view of this comparison. `files` names the two reports.
    pub(crate) fn view(
        &self,
        threshold: f64,
        files: Option<(String, String)>,
    ) -> ComparisonView<'_> {
        ComparisonView {
            threshold,
            window: self.window,
            regressions: &self.regressions,
            improvements: &self.improvements,
            source: ComparisonSource::Reports {
                files,
                baseline: &self.baseline,
                current: &self.current,
                series_compared: self.series_compared,
                compared: &self.compared,
            },
        }
    }
}

/// A comparison JSON (schema 1.x, as written by `--compare --format json`)
/// read back from a file.
#[cfg(feature = "compare")]
#[derive(Debug, Deserialize)]
pub(crate) struct SavedComparison {
    schema_version: String,
    threshold: f64,
    #[serde(default)]
    window: Option<SavedWindow>,
    regressions: Vec<Regression>,
    improvements: Vec<Improvement>,
}

#[cfg(feature = "compare")]
#[derive(Debug, Deserialize)]
struct SavedWindow {
    start_secs: f64,
    #[serde(default)]
    end_secs: Option<f64>,
}

#[cfg(feature = "compare")]
impl SavedComparison {
    /// Parses `json` when it is a comparison document: an object with
    /// `regressions` and `improvements` and no `metrics`. Returns `None` for
    /// anything else, such as a run report.
    pub(crate) fn detect(json: &str) -> Option<std::result::Result<Self, String>> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        if !is_comparison_document(&value) {
            return None;
        }
        Some(Self::from_value(value))
    }

    fn from_value(value: serde_json::Value) -> std::result::Result<Self, String> {
        match value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
        {
            Some(version) if version.starts_with("1.") => {}
            Some(version) => {
                return Err(format!(
                    "unsupported comparison schema version '{version}' (this wsblast reads 1.x)"
                ));
            }
            None => return Err("no schema_version field; not a wsblast comparison JSON".into()),
        }
        let saved: Self = serde_json::from_value(value)
            .map_err(|e| format!("not a wsblast comparison JSON: {e}"))?;
        if !saved.threshold.is_finite() || saved.threshold < 0.0 {
            return Err(format!(
                "not a wsblast comparison JSON: threshold {} is not a non-negative number",
                saved.threshold
            ));
        }
        saved.window()?;
        Ok(saved)
    }

    fn window(&self) -> std::result::Result<Option<CompareWindow>, String> {
        let Some(window) = &self.window else {
            return Ok(None);
        };
        let secs = |value: f64| {
            Duration::try_from_secs_f64(value)
                .map_err(|_| format!("not a wsblast comparison JSON: window bound {value} is not a non-negative number of seconds"))
        };
        Ok(Some(CompareWindow {
            start: Some(secs(window.start_secs)?).filter(|start| !start.is_zero()),
            end: window.end_secs.map(secs).transpose()?,
        }))
    }

    /// The view the renderers read; `timestamp` is shown in the HTML header.
    pub(crate) fn view(&self, timestamp: Option<(&'static str, String)>) -> ComparisonView<'_> {
        ComparisonView {
            threshold: self.threshold,
            window: self.window().ok().flatten(),
            regressions: &self.regressions,
            improvements: &self.improvements,
            source: ComparisonSource::Saved {
                schema_version: &self.schema_version,
                timestamp,
            },
        }
    }
}

/// Whether a JSON value has the shape of a comparison document rather than
/// a run report.
fn is_comparison_document(value: &serde_json::Value) -> bool {
    value.get("regressions").is_some()
        && value.get("improvements").is_some()
        && value.get("metrics").is_none()
}

/// Renders a comparison report in the specified format.
#[cfg(all(feature = "compare", test))]
pub(crate) fn render_comparison_report(
    comparison: &BaselineComparison,
    format: OutputFormat,
    threshold: f64,
) -> String {
    render_comparison_view(&comparison.view(threshold, None), format)
}

/// Renders a comparison view in `format`.
#[cfg(feature = "compare")]
pub(crate) fn render_comparison_view(view: &ComparisonView<'_>, format: OutputFormat) -> String {
    match format {
        OutputFormat::Text => render_text_comparison(view),
        OutputFormat::Json => render_json_comparison(view),
        OutputFormat::Markdown => render_markdown_comparison(view),
        OutputFormat::Html => crate::html::render_html_comparison(view),
    }
}

#[cfg(feature = "compare")]
fn render_text_comparison(comparison: &ComparisonView<'_>) -> String {
    let threshold = comparison.threshold;
    let mut out = String::new();

    out.push('\n');
    out.push_str(&format!(
        "{} {}\n\n",
        "wsblast".bold().cyan(),
        "Baseline Comparison Report".bold()
    ));

    out.push_str(&format!("Threshold: ±{:.1}%\n", threshold));
    if let Some(window) = &comparison.window {
        out.push_str(&format!("Window: {window} (time-series metrics only)\n"));
    }
    out.push('\n');

    if !comparison.regressions.is_empty() {
        let mut reg_table = Table::new();
        reg_table.load_preset(UTF8_FULL);
        reg_table.set_header(vec!["Metric", "Baseline", "Current", "Change", "Status"]);

        for r in comparison.regressions {
            reg_table.add_row(vec![
                &r.metric,
                &format!("{:.3}", r.baseline),
                &format!("{:.3}", r.current),
                &format!("{:+.2}%", r.pct_change),
                "⚠ REGRESSION",
            ]);
        }
        out.push_str(&reg_table.to_string());
        out.push_str("\n\n");
    }

    if !comparison.improvements.is_empty() {
        let mut imp_table = Table::new();
        imp_table.load_preset(UTF8_FULL);
        imp_table.set_header(vec!["Metric", "Baseline", "Current", "Change", "Status"]);

        for i in comparison.improvements {
            imp_table.add_row(vec![
                &i.metric,
                &format!("{:.3}", i.baseline),
                &format!("{:.3}", i.current),
                &format!("{:+.2}%", i.pct_change),
                "✓ IMPROVEMENT",
            ]);
        }
        out.push_str(&imp_table.to_string());
        out.push_str("\n\n");
    }

    if comparison.regressions.is_empty() && comparison.improvements.is_empty() {
        out.push_str("No significant changes detected beyond threshold.\n\n");
    }

    out
}

#[cfg(feature = "compare")]
fn render_json_comparison(comparison: &ComparisonView<'_>) -> String {
    #[derive(serde::Serialize)]
    struct JsonComparison<'a> {
        schema_version: &'a str,
        threshold: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<JsonWindow>,
        regressions: &'a [Regression],
        improvements: &'a [Improvement],
    }

    #[derive(serde::Serialize)]
    struct JsonWindow {
        start_secs: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        end_secs: Option<f64>,
    }

    // 1.2.0 adds the `series.*` metrics and `window`. A comparison that
    // uses neither, such as one of two 0.2.0 reports, is written exactly as
    // before, as 1.1.0.
    let schema_version = match &comparison.source {
        ComparisonSource::Saved { schema_version, .. } => schema_version,
        ComparisonSource::Reports {
            series_compared, ..
        } => {
            if *series_compared || comparison.window.is_some() {
                "1.2.0"
            } else {
                "1.1.0"
            }
        }
    };
    let report = JsonComparison {
        schema_version,
        threshold: comparison.threshold,
        window: comparison.window.map(|window| JsonWindow {
            start_secs: window.start.map_or(0.0, |start| start.as_secs_f64()),
            end_secs: window.end.map(|end| end.as_secs_f64()),
        }),
        regressions: comparison.regressions,
        improvements: comparison.improvements,
    };

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

#[cfg(feature = "compare")]
fn render_markdown_comparison(comparison: &ComparisonView<'_>) -> String {
    let threshold = comparison.threshold;
    let mut md = String::new();
    md.push_str("# `wsblast` Baseline Comparison Report\n\n");
    md.push_str(&format!("- **Threshold:** `±{:.1}%`\n", threshold));
    if let Some(window) = &comparison.window {
        md.push_str(&format!(
            "- **Window:** `{window}` (time-series metrics only)\n"
        ));
    }
    md.push('\n');

    if !comparison.regressions.is_empty() {
        md.push_str("## ⚠ Regressions Detected\n\n");
        md.push_str("| Metric | Baseline | Current | Change |\n");
        md.push_str("| :--- | ---: | ---: | ---: |\n");
        for r in comparison.regressions {
            md.push_str(&format!(
                "| {} | {:.3} | {:.3} | {:+.2}% |\n",
                r.metric, r.baseline, r.current, r.pct_change
            ));
        }
        md.push('\n');
    }

    if !comparison.improvements.is_empty() {
        md.push_str("## ✓ Improvements Detected\n\n");
        md.push_str("| Metric | Baseline | Current | Change |\n");
        md.push_str("| :--- | ---: | ---: | ---: |\n");
        for i in comparison.improvements {
            md.push_str(&format!(
                "| {} | {:.3} | {:.3} | {:+.2}% |\n",
                i.metric, i.baseline, i.current, i.pct_change
            ));
        }
        md.push('\n');
    }

    if comparison.regressions.is_empty() && comparison.improvements.is_empty() {
        md.push_str("No significant changes detected beyond threshold.\n");
    }

    md
}

/// Checks every threshold set in `slo` against `metrics`.
///
/// A latency threshold fails when no round trip was measured, because an
/// empty histogram is no evidence that the budget was met. Thresholds are
/// inclusive: a p99 equal to `max_p99` passes.
///
/// # Examples
///
/// Nothing listens on port 1, so this run measures no round trip and the
/// latency check fails along with the error budget. The short connect
/// timeout makes the attempt fail inside the run on every platform; Windows
/// otherwise retries a refused connection for about two seconds.
///
/// ```
/// use std::time::Duration;
/// use wsblast::{LoadTestConfig, Runner, SloThresholds, evaluate_slos};
///
/// # #[tokio::main]
/// # async fn main() -> wsblast::Result<()> {
/// let config = LoadTestConfig::builder("ws://127.0.0.1:1")
///     .connections(1)
///     .duration(Duration::from_millis(200))
///     .connect_timeout(Duration::from_millis(100))
///     .build()?;
/// let metrics = Runner::new(config).with_signal_handling(false).run().await?;
///
/// let slo = SloThresholds::new()
///     .with_max_p99(Duration::from_millis(50))
///     .with_max_error_rate(0.01);
/// let evaluation = evaluate_slos(&slo, &metrics);
///
/// assert!(!evaluation.passed);
/// assert_eq!(evaluation.checks[0].name, "Latency p99");
/// assert_eq!(evaluation.checks[0].actual, "no RTT samples");
/// assert!(!evaluation.checks[1].passed);
/// # Ok(())
/// # }
/// ```
pub fn evaluate_slos(slo: &SloThresholds, metrics: &AggregatedMetrics) -> SloEvaluation {
    let mut checks = Vec::new();
    let mut all_passed = true;

    // A paced run gates on response time, which counts the time a request
    // waited behind a slow reply; RTT alone would hide that wait.
    let (rtt, names) = if metrics.response_time.count > 0 {
        (
            &metrics.response_time,
            [
                "Response time p50",
                "Response time p95",
                "Response time p99",
                "Response time p99.9",
            ],
        )
    } else {
        (
            &metrics.message_rtt,
            ["Latency p50", "Latency p95", "Latency p99", "Latency p99.9"],
        )
    };
    let latency_targets = [
        (names[0], slo.max_p50, rtt.p50_duration()),
        (names[1], slo.max_p95, rtt.p95_duration()),
        (names[2], slo.max_p99, rtt.p99_duration()),
        (names[3], slo.max_p999, rtt.p999_duration()),
    ];
    for (name, target, actual) in latency_targets {
        let Some(target) = target else {
            continue;
        };
        let check = latency_check(name, target, actual, rtt.count);
        all_passed &= check.passed;
        checks.push(check);
    }

    if let Some(max_err) = slo.max_error_rate {
        let passed = metrics.error_rate <= max_err;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Error Rate".into(),
            target: format!("<= {:.2}%", max_err * 100.0),
            actual: format!("{:.2}%", metrics.error_rate * 100.0),
            passed,
        });
    }

    if let Some(max_lag) = slo.max_generator_lag {
        let check = match &metrics.generator_lag {
            Some(lag) => SloCheck {
                name: "Generator lag p99".into(),
                target: format!("<= {max_lag:?}"),
                actual: format!("{:?}", Duration::from_micros(lag.p99_us)),
                passed: Duration::from_micros(lag.p99_us) <= max_lag,
            },
            None => SloCheck {
                name: "Generator lag p99".into(),
                target: format!("<= {max_lag:?}"),
                actual: "no lag samples".into(),
                passed: false,
            },
        };
        all_passed &= check.passed;
        checks.push(check);
    }

    if let Some(min_tput) = slo.min_throughput {
        let passed = metrics.throughput_msg_per_sec >= min_tput;
        if !passed {
            all_passed = false;
        }
        checks.push(SloCheck {
            name: "Throughput (msg/s)".into(),
            target: format!(">= {:.1}", min_tput),
            actual: format!("{:.1}", metrics.throughput_msg_per_sec),
            passed,
        });
    }

    SloEvaluation {
        passed: all_passed,
        checks,
    }
}

/// Builds one latency SLO check.
///
/// A latency budget cannot pass without evidence: when no round trip was
/// measured (every request timed out, or the mode does not measure RTT) the
/// check fails instead of comparing the empty histogram's zero against the target.
/// The most rows the text and Markdown reports show for the time series.
const MAX_TIME_WINDOWS: usize = 10;

/// A group of consecutive time series points, as one report row.
struct TimeWindow {
    label: String,
    throughput: f64,
    errors: u64,
    /// The highest p99 of any interval in the window. Percentiles cannot be
    /// combined, so this is the worst interval, not the window's p99.
    worst_p99: Option<u64>,
}

/// Groups the run's time series into at most [`MAX_TIME_WINDOWS`] rows. A
/// series with fewer than two points adds nothing to the summary tables, so
/// it gives no rows.
fn time_windows(metrics: &AggregatedMetrics) -> Vec<TimeWindow> {
    let points = &metrics.time_series.points;
    if points.len() < 2 {
        return Vec::new();
    }
    let per_window = points.len().div_ceil(MAX_TIME_WINDOWS);
    points
        .chunks(per_window)
        .map(|chunk| {
            let start = chunk[0].start_secs;
            let last = &chunk[chunk.len() - 1];
            let end = last.start_secs + last.duration_secs;
            let secs: f64 = chunk.iter().map(|p| p.duration_secs).sum();
            let messages: u64 = chunk
                .iter()
                .map(|p| p.messages_sent.max(p.messages_recv))
                .sum();
            TimeWindow {
                label: format!("{} to {}", format_secs(start), format_secs(end)),
                throughput: messages as f64 / secs.max(0.000_1),
                errors: chunk.iter().map(|p| p.errors).sum(),
                worst_p99: chunk
                    .iter()
                    .filter(|p| p.latency.count > 0)
                    .map(|p| p.latency.p99_us)
                    .max(),
            }
        })
        .collect()
}

/// The text report's rate row: the per-connection rate, `Unthrottled`, or a
/// note that a saved report does not record it.
fn rate_label(rate_per_conn: Option<u64>) -> String {
    match rate_per_conn {
        Some(0) => "Unthrottled".to_string(),
        Some(rate) => format!("{rate} msg/sec per connection"),
        None => "not recorded in the JSON report".to_string(),
    }
}

/// Formats seconds since the run started, with one decimal only when the
/// value is not a whole number of seconds.
pub(crate) fn format_secs(secs: f64) -> String {
    if (secs - secs.round()).abs() < 0.05 {
        format!("{:.0}s", secs.round())
    } else {
        format!("{secs:.1}s")
    }
}

pub(crate) fn latency_basis_label(metrics: &AggregatedMetrics) -> &'static str {
    match metrics.time_series.latency_basis.as_str() {
        "response_time" => "Response Time",
        "mixed" => "Latency",
        _ => "RTT",
    }
}

/// A report row label and the percentile it reads from a distribution.
pub(crate) type PercentileRow = (&'static str, fn(&LatencyStats) -> u64);

/// The latency distributions a report shows, by column name. Response time
/// appears only when the run recorded it.
pub(crate) fn latency_columns(metrics: &AggregatedMetrics) -> Vec<(&'static str, &LatencyStats)> {
    let mut columns = vec![
        ("Handshake Latency", &metrics.handshake_latency),
        ("Message RTT Latency", &metrics.message_rtt),
    ];
    if metrics.response_time.count > 0 {
        columns.push(("Response Time (from schedule)", &metrics.response_time));
    }
    columns
}

fn latency_check(name: &str, target: Duration, actual: Duration, samples: u64) -> SloCheck {
    if samples == 0 {
        return SloCheck {
            name: name.into(),
            target: format!("<= {target:?}"),
            actual: "no RTT samples".into(),
            passed: false,
        };
    }
    SloCheck {
        name: name.into(),
        target: format!("<= {target:?}"),
        actual: format!("{actual:?}"),
        passed: actual <= target,
    }
}

/// What a report shows about the run besides its metrics and SLO result.
///
/// A live run builds it from its [`LoadTestConfig`]
/// ([`ReportView::from_config`]); `wsblast render` builds it from a saved JSON
/// report ([`SavedReport::view`]), which records less of the configuration,
/// so the fields a JSON report lacks are optional.
pub(crate) struct ReportView<'a> {
    /// Target URL after parsing; for a scenario, the first phase's.
    pub(crate) target_url: &'a str,
    /// Configured connection count; for a scenario, the sum over phases.
    pub(crate) connections: usize,
    /// Session mode, in Rust Debug formatting (`Echo`), as the JSON report writes it.
    pub(crate) mode: Cow<'a, str>,
    /// Messages per second per connection, 0 for unthrottled; `None` when
    /// the source does not record it.
    pub(crate) rate_per_conn: Option<u64>,
    /// The `--stages` profile; empty for a constant connection count or
    /// when the source does not record it.
    pub(crate) stages: Vec<StageReport>,
    /// Every executed scenario phase; empty outside scenarios.
    pub(crate) phase_targets: &'a [PhaseTarget],
    /// The metrics and SLO result of every executed scenario phase; empty
    /// outside scenarios and for reports written before they were recorded.
    pub(crate) phases: &'a [PhaseReport],
    /// A labelled wall-clock time for the HTML header, such as when the run
    /// finished.
    pub(crate) timestamp: Option<(&'static str, String)>,
    pub(crate) metrics: &'a AggregatedMetrics,
    pub(crate) slo: &'a SloEvaluation,
}

impl<'a> ReportView<'a> {
    /// The `--stages` profile as `DURATION:TARGET` pairs, if the run had one
    /// and the source records it.
    pub(crate) fn stages_label(&self) -> Option<String> {
        (!self.stages.is_empty()).then(|| stage_reports_label(&self.stages))
    }

    /// The view of a live run.
    pub(crate) fn from_config(
        config: &'a LoadTestConfig,
        metrics: &'a AggregatedMetrics,
        slo: &'a SloEvaluation,
        phase_targets: &'a [PhaseTarget],
        phases: &'a [PhaseReport],
    ) -> Self {
        Self {
            target_url: config.target_url.as_str(),
            connections: config.connections,
            mode: Cow::Owned(format!("{:?}", config.mode)),
            rate_per_conn: Some(config.rate_per_conn),
            stages: config
                .stages
                .iter()
                .map(|stage| StageReport {
                    duration: stage.duration,
                    target: stage.target,
                })
                .collect(),
            phase_targets,
            phases,
            timestamp: Some((
                "Finished",
                crate::clock::rfc3339_utc(std::time::SystemTime::now()),
            )),
            metrics,
            slo,
        }
    }
}

/// A JSON report (schema 1.x) read back from a file, as written by
/// `--format json`. Fields added after 0.2.0 default when absent, and
/// `schema_version` is checked by [`SavedReport::parse`].
#[derive(Debug, Deserialize)]
pub(crate) struct SavedReport {
    target_url: String,
    connections: usize,
    mode: String,
    /// Absent in reports written before 0.2.2 and in scenario reports.
    #[serde(default)]
    rate_per_conn: Option<u64>,
    #[serde(default)]
    stages: Vec<StageReport>,
    #[serde(default)]
    phase_targets: Vec<PhaseTarget>,
    metrics: AggregatedMetrics,
    slo: SloEvaluation,
    #[serde(default)]
    phases: Vec<PhaseReport>,
}

impl SavedReport {
    /// Parses a saved report, rejecting JSON that is not a wsblast schema
    /// 1.x report.
    pub(crate) fn parse(json: &str) -> std::result::Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
        match value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
        {
            Some(version) if version.starts_with("1.") => {}
            Some(version) => {
                return Err(format!(
                    "unsupported report schema version '{version}' (this wsblast reads 1.x)"
                ));
            }
            None => return Err("no schema_version field; not a wsblast JSON report".into()),
        }
        if is_comparison_document(&value) {
            return Err(
                "a --compare report; rendering one needs the 'compare' feature, which this build of wsblast does not include"
                    .into(),
            );
        }
        serde_json::from_value(value).map_err(|e| format!("not a wsblast JSON report: {e}"))
    }

    /// The view the renderers read. Reports written before 0.2.2 do not
    /// record the message rate or the load stages, so both are unknown.
    pub(crate) fn view(&self, timestamp: Option<(&'static str, String)>) -> ReportView<'_> {
        ReportView {
            target_url: &self.target_url,
            connections: self.connections,
            mode: Cow::Borrowed(&self.mode),
            rate_per_conn: self.rate_per_conn,
            stages: self.stages.clone(),
            phase_targets: &self.phase_targets,
            phases: &self.phases,
            timestamp,
            metrics: &self.metrics,
            slo: &self.slo,
        }
    }
}

/// Renders the run report in the configuration's
/// [`output_format`](LoadTestConfig::output_format).
///
/// This is the report the CLI prints: terminal tables for
/// [`OutputFormat::Text`], the schema 1.0 document for [`OutputFormat::Json`],
/// a GitHub Flavored Markdown summary for [`OutputFormat::Markdown`], and a
/// self-contained HTML page with charts of the time series for
/// [`OutputFormat::Html`]. The JSON schema is published in the repository as
/// [`schema/report-v1.json`](https://github.com/jephter-olamiposi/wsblast/blob/main/schema/report-v1.json).
pub fn render_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
) -> String {
    render_scenario_report(config, metrics, slo, &[], &[])
}

/// Formats a report that also lists the target, and the results, of every
/// executed scenario phase. With no phases it is identical to
/// [`render_report`].
pub(crate) fn render_scenario_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
    phases: &[PhaseReport],
) -> String {
    render_view(
        &ReportView::from_config(config, metrics, slo, phase_targets, phases),
        config.output_format,
    )
}

/// Renders `view` in `format`.
pub(crate) fn render_view(view: &ReportView<'_>, format: OutputFormat) -> String {
    match format {
        OutputFormat::Text => render_text_report(view),
        OutputFormat::Json => render_json_report(view),
        OutputFormat::Markdown => render_markdown_report(view),
        OutputFormat::Html => crate::html::render_html_report(view),
    }
}

/// Emits the report of a finished run, listing the target of every executed
/// scenario phase.
///
/// Without an output path the report is printed to stdout in the configured
/// format. With one it is written to that file, and stdout gets the text
/// summary, so a terminal or CI log stays readable whatever the file's
/// format.
///
/// # Errors
///
/// Returns [`std::io::Error`] if writing to the specified `output_path` fails.
pub(crate) fn emit_scenario_report(
    config: &LoadTestConfig,
    metrics: &AggregatedMetrics,
    slo: &SloEvaluation,
    phase_targets: &[PhaseTarget],
    phases: &[PhaseReport],
) -> std::io::Result<()> {
    let view = ReportView::from_config(config, metrics, slo, phase_targets, phases);
    let output = render_view(&view, config.output_format);
    match &config.output_path {
        Some(path) => {
            fs::write(path, &output)?;
            let summary = if config.output_format == OutputFormat::Text {
                output
            } else {
                render_view(&view, OutputFormat::Text)
            };
            println!("{summary}");
        }
        None => println!("{output}"),
    }
    Ok(())
}

fn render_text_report(view: &ReportView<'_>) -> String {
    let (metrics, slo, phase_targets) = (view.metrics, view.slo, view.phase_targets);
    let mut out = String::new();

    out.push('\n');
    out.push_str(&format!(
        "{} {}\n\n",
        "wsblast".bold().cyan(),
        "WebSocket Benchmark Summary".bold()
    ));

    let mut meta_table = Table::new();
    meta_table.load_preset(UTF8_FULL);
    meta_table.set_header(vec!["Configuration Parameter", "Value"]);
    if phase_targets.is_empty() {
        meta_table.add_row(vec!["Target URL", view.target_url]);
    }
    for target in phase_targets {
        meta_table.add_row(vec![
            format!("Target URL (phase '{}')", target.phase),
            target.target_url.to_string(),
        ]);
    }
    meta_table.add_row(vec![
        "Connections (Concurrency)",
        &view.connections.to_string(),
    ]);
    if let Some(stages) = &view.stages_label() {
        meta_table.add_row(vec!["Load Stages", stages]);
    }
    meta_table.add_row(vec![
        "Elapsed Duration",
        &format!("{:.2?}", metrics.elapsed),
    ]);
    meta_table.add_row(vec!["Rate Control", &rate_label(view.rate_per_conn)]);
    meta_table.add_row(vec!["Mode", &view.mode]);
    if let Some(distributed) = &metrics.distributed {
        meta_table.add_row(vec![
            "Agents",
            &agents_label(distributed, |a| a.to_string()),
        ]);
    }
    out.push_str(&meta_table.to_string());
    out.push_str("\n\n");

    let mut tput_table = Table::new();
    tput_table.load_preset(UTF8_FULL);
    tput_table.set_header(vec!["Traffic & Throughput Metric", "Count / Rate"]);
    tput_table.add_row(vec![
        "Connections Established / Failed",
        &format!(
            "{}/{}",
            metrics.total_connections_established, metrics.total_connections_failed
        ),
    ]);
    if metrics.total_connections_interrupted > 0 {
        tput_table.add_row(vec![
            "Connections Interrupted (still connecting at end of run)",
            &metrics.total_connections_interrupted.to_string(),
        ]);
    }
    if metrics.total_connections_not_started > 0 {
        tput_table.add_row(vec![
            "Connections Not Started (ramp cut short by --duration)",
            &metrics.total_connections_not_started.to_string(),
        ]);
    }
    tput_table.add_row(vec!["Reconnects", &metrics.total_reconnects.to_string()]);
    if metrics.total_unsolicited_frames > 0 {
        tput_table.add_row(vec![
            "Unsolicited Frames (skipped by --match)",
            &metrics.total_unsolicited_frames.to_string(),
        ]);
    }
    tput_table.add_row(vec![
        "Messages Sent / Received",
        &format!(
            "{} / {}",
            metrics.total_messages_sent, metrics.total_messages_recv
        ),
    ]);
    tput_table.add_row(vec![
        "Data Transferred (Sent / Recv)",
        &format!(
            "{:.2} MB / {:.2} MB",
            metrics.total_bytes_sent as f64 / (1024.0 * 1024.0),
            metrics.total_bytes_recv as f64 / (1024.0 * 1024.0)
        ),
    ]);
    tput_table.add_row(vec![
        "Throughput (Messages/sec)",
        &format!("{:.2} msg/sec", metrics.throughput_msg_per_sec),
    ]);
    tput_table.add_row(vec![
        "Bandwidth (Transfer Rate)",
        &format!(
            "{:.2} MB/sec",
            metrics.throughput_bytes_per_sec / (1024.0 * 1024.0)
        ),
    ]);
    tput_table.add_row(vec![
        "Error Rate",
        &format!("{:.2}%", metrics.error_rate * 100.0),
    ]);
    out.push_str(&tput_table.to_string());
    out.push_str("\n\n");
    if let Some(capture) = &metrics.capture {
        out.push_str(&format!("{}\n\n", capture_line(capture)));
    }
    if let Some(chaos) = &metrics.chaos {
        out.push_str(&format!("Chaos: {}\n\n", chaos_label(chaos)));
    }
    if let Some(generator) = generator_label(metrics) {
        out.push_str(&format!("Generator: {generator}\n\n"));
    }

    let columns = latency_columns(metrics);
    let mut lat_table = Table::new();
    lat_table.load_preset(UTF8_FULL);
    let mut header = vec!["Percentile"];
    header.extend(columns.iter().map(|(name, _)| *name));
    lat_table.set_header(header);
    let rows: [PercentileRow; 6] = [
        ("p50 (Median)", |s| s.p50_us),
        ("p75", |s| s.p75_us),
        ("p90", |s| s.p90_us),
        ("p95", |s| s.p95_us),
        ("p99", |s| s.p99_us),
        ("p99.9", |s| s.p999_us),
    ];
    for (label, value) in rows {
        let mut row = vec![label.to_string()];
        row.extend(columns.iter().map(|(_, stats)| format_us(value(stats))));
        lat_table.add_row(row);
    }
    let mut row = vec!["Min / Mean / Max".to_string()];
    row.extend(columns.iter().map(|(_, stats)| {
        format!(
            "{} / {} / {}",
            format_us(stats.min_us),
            format_us(stats.mean_us as u64),
            format_us(stats.max_us)
        )
    }));
    lat_table.add_row(row);
    out.push_str(&lat_table.to_string());
    out.push_str("\n\n");

    let windows = time_windows(metrics);
    if !windows.is_empty() {
        let mut ts_table = Table::new();
        ts_table.load_preset(UTF8_FULL);
        ts_table.set_header(vec![
            "Time Window".to_string(),
            "Throughput (msg/s)".to_string(),
            "Errors".to_string(),
            format!("Worst {} p99", latency_basis_label(metrics)),
        ]);
        for w in &windows {
            ts_table.add_row(vec![
                w.label.clone(),
                format!("{:.1}", w.throughput),
                w.errors.to_string(),
                w.worst_p99.map_or_else(|| "-".to_string(), format_us),
            ]);
        }
        out.push_str(&ts_table.to_string());
        out.push_str("\n\n");
    }

    if !metrics.error_breakdown.is_empty() {
        let mut err_table = Table::new();
        err_table.load_preset(UTF8_FULL);
        err_table.set_header(vec!["Error Taxonomy Category", "Occurrences"]);
        for (cat, count) in &metrics.error_breakdown {
            err_table.add_row(vec![cat.to_string(), count.to_string()]);
        }
        out.push_str(&err_table.to_string());
        out.push_str("\n\n");
    }

    if shows_phase_table(view.phases) {
        let mut phase_table = Table::new();
        phase_table.load_preset(UTF8_FULL);
        phase_table.set_header(PHASE_COLUMNS.to_vec());
        for phase in view.phases {
            let status = match phase.slo_label() {
                "PASSED" => Cell::new("PASSED").fg(Color::Green),
                "FAILED" => Cell::new("FAILED").fg(Color::Red),
                other => Cell::new(other),
            };
            phase_table.add_row(Row::from(vec![
                Cell::new(&phase.name),
                Cell::new(phase.target_url.as_str()),
                Cell::new(phase.load_label()),
                Cell::new(format!("{:.1}", phase.metrics.throughput_msg_per_sec)),
                Cell::new(format!("{:.2}%", phase.metrics.error_rate * 100.0)),
                Cell::new(phase.gating_p99_label()),
                status,
            ]));
        }
        out.push_str(&phase_table.to_string());
        out.push_str("\n\n");
    }

    if !slo.checks.is_empty() {
        let mut slo_table = Table::new();
        slo_table.load_preset(UTF8_FULL);
        slo_table.set_header(vec![
            "SLO Objective",
            "Target Threshold",
            "Actual Value",
            "Gate Status",
        ]);

        for c in &slo.checks {
            let status_cell = if c.passed {
                Cell::new("PASSED").fg(Color::Green)
            } else {
                Cell::new("FAILED").fg(Color::Red)
            };
            slo_table.add_row(Row::from(vec![
                Cell::new(&c.name),
                Cell::new(&c.target),
                Cell::new(&c.actual),
                status_cell,
            ]));
        }
        out.push_str(&slo_table.to_string());
        out.push_str("\n\n");

        if slo.passed {
            out.push_str(&format!(
                "{} {}\n",
                "[PASS]".bold().green(),
                "All SLO thresholds satisfied."
            ));
        } else {
            out.push_str(&format!(
                "{} {}\n",
                "[FAIL]".bold().red(),
                "One or more SLO thresholds breached!"
            ));
        }
    }

    out
}

/// The columns of the per-phase table of the text and Markdown reports.
const PHASE_COLUMNS: [&str; 7] = [
    "Phase",
    "Target",
    "Load",
    "Throughput (msg/s)",
    "Error Rate",
    "Gating p99",
    "SLO",
];

#[derive(Debug, serde::Serialize)]
struct JsonReport<'a> {
    schema_version: &'static str,
    target_url: &'a str,
    connections: usize,
    duration_secs: f64,
    mode: &'a str,
    /// Messages per second per connection, 0 for unthrottled; omitted for a
    /// scenario, whose phases set their own rates.
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_per_conn: Option<u64>,
    /// The `--stages` profile; omitted for a constant connection count.
    #[serde(skip_serializing_if = "<[StageReport]>::is_empty")]
    stages: &'a [StageReport],
    /// Every executed scenario phase in run order; omitted outside scenarios.
    #[serde(skip_serializing_if = "<[PhaseTarget]>::is_empty")]
    phase_targets: &'a [PhaseTarget],
    metrics: &'a AggregatedMetrics,
    slo: &'a SloEvaluation,
    /// Every executed scenario phase with its own metrics and SLO result;
    /// omitted outside scenarios.
    #[serde(skip_serializing_if = "<[PhaseReport]>::is_empty")]
    phases: &'a [PhaseReport],
}

fn render_json_report(view: &ReportView<'_>) -> String {
    let report = JsonReport {
        schema_version: "1.0.0",
        target_url: view.target_url,
        connections: view.connections,
        duration_secs: view.metrics.elapsed.as_secs_f64(),
        mode: &view.mode,
        rate_per_conn: view
            .rate_per_conn
            .filter(|_| view.phase_targets.is_empty() && view.phases.is_empty()),
        stages: &view.stages,
        phase_targets: view.phase_targets,
        metrics: view.metrics,
        slo: view.slo,
        phases: view.phases,
    };

    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}

fn render_markdown_report(view: &ReportView<'_>) -> String {
    let (metrics, slo, phase_targets) = (view.metrics, view.slo, view.phase_targets);
    let mut md = String::new();
    md.push_str("# `wsblast` WebSocket Benchmark Report\n\n");
    if phase_targets.is_empty() {
        md.push_str(&format!("- **Target:** `{}`\n", view.target_url));
    } else {
        md.push_str("- **Targets:**\n");
        for target in phase_targets {
            md.push_str(&format!(
                "  - phase `{}`: `{}`\n",
                target.phase, target.target_url
            ));
        }
    }
    md.push_str(&format!(
        "- **Concurrency:** `{}` workers\n",
        view.connections
    ));
    if let Some(stages) = &view.stages_label() {
        md.push_str(&format!("- **Load Stages:** `{stages}`\n"));
    }
    if let Some(distributed) = &metrics.distributed {
        md.push_str(&format!(
            "- **Agents:** {}\n",
            agents_label(distributed, |a| format!("`{a}`"))
        ));
    }
    md.push_str(&format!(
        "- **Connections:** `{}` established / `{}` failed / `{}` reconnects\n",
        metrics.total_connections_established,
        metrics.total_connections_failed,
        metrics.total_reconnects
    ));
    if metrics.total_connections_interrupted > 0 {
        md.push_str(&format!(
            "- **Connections Interrupted:** `{}` (still connecting at end of run)\n",
            metrics.total_connections_interrupted
        ));
    }
    if metrics.total_connections_not_started > 0 {
        md.push_str(&format!(
            "- **Connections Not Started:** `{}` (ramp cut short by `--duration`)\n",
            metrics.total_connections_not_started
        ));
    }
    if metrics.total_unsolicited_frames > 0 {
        md.push_str(&format!(
            "- **Unsolicited Frames:** `{}` skipped by `--match`\n",
            metrics.total_unsolicited_frames
        ));
    }
    md.push_str(&format!("- **Duration:** `{:.2?}`\n", metrics.elapsed));
    md.push_str(&format!(
        "- **Throughput:** `{:.2}` msg/sec\n",
        metrics.throughput_msg_per_sec
    ));
    md.push_str(&format!(
        "- **Error Rate:** `{:.2}%`\n",
        metrics.error_rate * 100.0
    ));
    if let Some(capture) = &metrics.capture {
        md.push_str(&format!(
            "- **Capture:** {} frames written, {} dropped (`{}`)\n",
            capture.frames_written, capture.frames_dropped, capture.path
        ));
    }
    if let Some(chaos) = &metrics.chaos {
        md.push_str(&format!("- **Chaos:** {}\n", chaos_label(chaos)));
    }
    if let Some(generator) = generator_label(metrics) {
        md.push_str(&format!("- **Generator:** {generator}\n"));
    }
    md.push('\n');

    md.push_str("### Latency Distribution\n\n");
    let columns = latency_columns(metrics);
    md.push_str("| Percentile |");
    for (name, _) in &columns {
        md.push_str(&format!(" {name} |"));
    }
    md.push_str("\n| :--- |");
    md.push_str(&" :--- |".repeat(columns.len()));
    md.push('\n');
    let rows: [PercentileRow; 6] = [
        ("p50", |s| s.p50_us),
        ("p90", |s| s.p90_us),
        ("p95", |s| s.p95_us),
        ("p99", |s| s.p99_us),
        ("p99.9", |s| s.p999_us),
        ("Max", |s| s.max_us),
    ];
    for (label, value) in rows {
        md.push_str(&format!("| **{label}** |"));
        for (_, stats) in &columns {
            md.push_str(&format!(" {} |", format_us(value(stats))));
        }
        md.push('\n');
    }
    md.push('\n');

    let windows = time_windows(metrics);
    if !windows.is_empty() {
        md.push_str("### Over Time\n\n");
        md.push_str(&format!(
            "| Time Window | Throughput (msg/s) | Errors | Worst {} p99 |\n",
            latency_basis_label(metrics)
        ));
        md.push_str("| :--- | :--- | :--- | :--- |\n");
        for w in &windows {
            md.push_str(&format!(
                "| {} | {:.1} | {} | {} |\n",
                w.label,
                w.throughput,
                w.errors,
                w.worst_p99.map_or_else(|| "-".to_string(), format_us)
            ));
        }
        md.push('\n');
    }

    if shows_phase_table(view.phases) {
        md.push_str("### Phases\n\n|");
        for column in PHASE_COLUMNS {
            md.push_str(&format!(" {column} |"));
        }
        md.push_str("\n|");
        md.push_str(&" :--- |".repeat(PHASE_COLUMNS.len()));
        md.push('\n');
        for phase in view.phases {
            let status = match phase.slo_label() {
                "PASSED" => "✅ PASS",
                "FAILED" => "❌ FAIL",
                other => other,
            };
            md.push_str(&format!(
                "| `{}` | `{}` | {} | {:.1} | {:.2}% | {} | **{}** |\n",
                phase.name,
                phase.target_url,
                phase.load_label(),
                phase.metrics.throughput_msg_per_sec,
                phase.metrics.error_rate * 100.0,
                phase.gating_p99_label(),
                status
            ));
        }
        md.push('\n');
    }

    if !slo.checks.is_empty() {
        md.push_str("### SLO Gate Evaluation\n\n");
        md.push_str("| Metric | Target | Actual | Status |\n");
        md.push_str("| :--- | :--- | :--- | :--- |\n");
        for c in &slo.checks {
            let icon = if c.passed { "✅ PASS" } else { "❌ FAIL" };
            md.push_str(&format!(
                "| {} | `{}` | `{}` | **{}** |\n",
                c.name, c.target, c.actual, icon
            ));
        }
    }

    md
}

/// "N" or "N (lost: a, b)" for a distributed run, each lost agent rendered
/// by `name`.
pub(crate) fn agents_label(
    distributed: &DistributedSummary,
    name: impl Fn(&str) -> String,
) -> String {
    let count = distributed.agents.len();
    if distributed.lost_agents.is_empty() {
        return count.to_string();
    }
    let lost: Vec<String> = distributed.lost_agents.iter().map(|a| name(a)).collect();
    format!("{count} (lost: {})", lost.join(", "))
}

/// The injected faults of a run, as the text, Markdown and HTML reports
/// show them.
pub(crate) fn chaos_label(chaos: &ChaosSummary) -> String {
    format!(
        "{} delayed, {} dropped, {} corrupted, {} closes (seed {}; not counted as errors)",
        chaos.delayed, chaos.dropped, chaos.corrupted, chaos.closes, chaos.seed
    )
}

/// The health of the load generator, as the text, Markdown and HTML
/// reports show it: its wake-up lag and its open file limit. `None` when
/// the run measured neither.
///
/// In a distributed run the lag is the highest of the agents' and the
/// limit the tightest, each named with its agent, and the agents whose
/// generator was saturated, or whose limit was too low, are listed.
pub(crate) fn generator_label(metrics: &AggregatedMetrics) -> Option<String> {
    let distributed = metrics
        .distributed
        .as_ref()
        .filter(|summary| !summary.generators.is_empty());
    let of_agents = |agent: Option<&String>| match (agent, distributed) {
        (Some(agent), Some(summary)) => {
            format!(
                " on agent {agent}, the highest of {} agents",
                summary.generators.len()
            )
        }
        _ => String::new(),
    };
    let lag = metrics.generator_lag.as_ref().map(|lag| {
        let saturated: Vec<&str> = distributed
            .map(|summary| summary.saturated_agents().collect())
            .unwrap_or_default();
        let verdict = match (lag.saturated, saturated.is_empty()) {
            (false, _) => "ok".to_string(),
            (true, true) => "SATURATED: latency includes client-side delay".to_string(),
            (true, false) => format!(
                "SATURATED on {}: latency includes client-side delay",
                saturated.join(", ")
            ),
        };
        format!(
            "lag p99 {}, max {} over {} wake-ups{} (threshold {}; {verdict})",
            format_us(lag.p99_us),
            format_us(lag.max_us),
            lag.samples,
            of_agents(distributed.and_then(|summary| summary.generator_lag_agent.as_ref())),
            format_us(lag.threshold_us)
        )
    });
    let files = metrics.open_file_limit.as_ref().map(|limit| {
        let short: Vec<&str> = distributed
            .map(|summary| summary.agents_short_of_files().collect())
            .unwrap_or_default();
        let verdict = match (limit.sufficient, short.is_empty()) {
            (true, _) => String::new(),
            (false, true) => " (too low)".to_string(),
            (false, false) => format!(" (too low on {})", short.join(", ")),
        };
        let agent = match (
            distributed.and_then(|summary| summary.open_file_limit_agent.as_ref()),
            distributed,
        ) {
            (Some(agent), Some(summary)) => format!(
                " on agent {agent}, the tightest of {} agents",
                summary.generators.len()
            ),
            _ => String::new(),
        };
        format!(
            "open file limit {} for {} needed{agent}{verdict}",
            limit.soft_limit, limit.needed,
        )
    });
    match (lag, files) {
        (None, None) => None,
        (Some(one), None) | (None, Some(one)) => Some(one),
        (Some(lag), Some(files)) => Some(format!("{lag}; {files}")),
    }
}

/// The capture totals line of the text report.
fn capture_line(capture: &CaptureSummary) -> String {
    format!(
        "Capture: {} frames written, {} dropped ({})",
        capture.frames_written, capture.frames_dropped, capture.path
    )
}

pub(crate) fn format_us(us: u64) -> String {
    if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{:.2} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.3} s", us as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "compare")]
    use crate::error::ErrorCategory;
    use crate::metrics::LatencyStats;
    use std::collections::HashMap;

    fn latency(value: u64) -> LatencyStats {
        LatencyStats {
            count: 1,
            min_us: value,
            mean_us: value as f64,
            max_us: value,
            std_dev_us: 0.0,
            p50_us: value,
            p75_us: value,
            p90_us: value,
            p95_us: value,
            p99_us: value,
            p999_us: value,
            p9999_us: value,
        }
    }

    fn sample_metrics() -> AggregatedMetrics {
        AggregatedMetrics {
            total_connections_attempted: 10,
            total_connections_established: 10,
            total_connections_failed: 0,
            total_connections_interrupted: 0,
            total_connections_not_started: 0,
            total_reconnects: 0,
            total_unsolicited_frames: 0,
            total_messages_sent: 100,
            total_messages_recv: 100,
            total_bytes_sent: 1_000,
            total_bytes_recv: 1_000,
            elapsed: std::time::Duration::from_secs(1),
            throughput_msg_per_sec: 100.0,
            throughput_bytes_per_sec: 2_000.0,
            error_rate: 0.0,
            handshake_latency: latency(500),
            message_rtt: latency(250),
            response_time: LatencyStats::default(),
            time_series: crate::timeseries::TimeSeries::default(),
            error_breakdown: HashMap::new(),
            capture: None,
            distributed: None,
            chaos: None,
            generator_lag: None,
            open_file_limit: None,
        }
    }

    #[test]
    fn latency_slo_fails_when_no_rtt_was_measured() {
        let mut metrics = sample_metrics();
        metrics.message_rtt = LatencyStats::from_histogram(
            &hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
        );
        let slo = SloThresholds {
            max_p50: Some(Duration::from_secs(1)),
            max_p99: Some(Duration::from_secs(1)),
            ..Default::default()
        };

        let evaluation = evaluate_slos(&slo, &metrics);

        assert!(!evaluation.passed);
        assert_eq!(evaluation.checks.len(), 2);
        assert!(evaluation.checks.iter().all(|check| !check.passed));
        assert!(
            evaluation
                .checks
                .iter()
                .all(|check| check.actual == "no RTT samples")
        );
    }

    #[test]
    fn latency_slo_compares_measured_percentiles() {
        let metrics = sample_metrics();
        let within = SloThresholds {
            max_p99: Some(Duration::from_micros(250)),
            ..Default::default()
        };
        let breached = SloThresholds {
            max_p99: Some(Duration::from_micros(249)),
            ..Default::default()
        };

        assert!(evaluate_slos(&within, &metrics).passed);
        assert!(!evaluate_slos(&breached, &metrics).passed);
    }

    #[test]
    fn non_latency_slos_still_evaluate_without_rtt_samples() {
        let mut metrics = sample_metrics();
        metrics.message_rtt = LatencyStats::from_histogram(
            &hdrhistogram::Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap(),
        );
        let slo = SloThresholds {
            max_error_rate: Some(0.01),
            min_throughput: Some(50.0),
            ..Default::default()
        };

        assert!(evaluate_slos(&slo, &metrics).passed);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn higher_throughput_is_an_improvement_and_lower_is_a_regression() {
        let baseline = sample_metrics();

        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        assert!(comparison.regressions.is_empty());
        assert!(
            comparison
                .improvements
                .iter()
                .any(|item| item.metric == "throughput_msg_per_sec")
        );

        let mut slower = sample_metrics();
        slower.throughput_msg_per_sec = 50.0;
        let comparison = compare_metrics(&baseline, &slower, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "throughput_msg_per_sec")
        );
        assert!(
            comparison
                .improvements
                .iter()
                .all(|item| item.metric != "throughput_msg_per_sec")
        );
    }

    #[cfg(feature = "compare")]
    #[test]
    fn higher_latency_is_a_regression_and_lower_is_an_improvement() {
        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);

        let mut faster = sample_metrics();
        faster.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        assert!(
            comparison
                .improvements
                .iter()
                .any(|item| item.metric == "message_rtt.p50_us")
        );

        let mut slower = sample_metrics();
        slower.message_rtt = latency(2_000);
        let comparison = compare_metrics(&baseline, &slower, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "message_rtt.p50_us")
        );
    }

    #[cfg(feature = "compare")]
    #[test]
    fn new_errors_from_a_zero_baseline_are_regressions() {
        let baseline = sample_metrics();

        let mut current = sample_metrics();
        current.total_connections_failed = 2;
        current.error_rate = 0.2;
        current
            .error_breakdown
            .insert(ErrorCategory::DnsResolution, 2);

        let comparison = compare_metrics(&baseline, &current, 10.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "total_connections_failed")
        );
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "error_rate")
        );
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "errors.dns_resolution")
        );
    }

    #[cfg(feature = "compare")]
    #[test]
    fn parses_schema_envelopes_and_bare_metrics() {
        let metrics = sample_metrics();
        let envelope = serde_json::json!({
            "schema_version": "1.0.0",
            "metrics": metrics,
        });

        let parsed = parse_report_metrics(&envelope.to_string()).expect("envelope must parse");
        assert_eq!(parsed.total_messages_sent, metrics.total_messages_sent);

        let parsed = parse_report_metrics(&serde_json::to_string(&metrics).expect("serialize"))
            .expect("bare metrics must parse");
        assert_eq!(
            parsed.throughput_msg_per_sec,
            metrics.throughput_msg_per_sec
        );
    }

    #[cfg(feature = "compare")]
    fn rows_for_metric(rendered: &str, metric: &str) -> Vec<String> {
        rendered
            .lines()
            .filter(|line| line.contains(metric))
            .map(str::to_owned)
            .collect()
    }

    #[cfg(feature = "compare")]
    #[test]
    fn comparison_reports_print_signed_improvement_change() {
        let baseline = sample_metrics();
        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);

        for format in [OutputFormat::Text, OutputFormat::Markdown] {
            let rendered = render_comparison_report(&comparison, format, 10.0);
            let rows = rows_for_metric(&rendered, "throughput_msg_per_sec");
            assert_eq!(rows.len(), 1, "{rendered}");
            assert!(rows[0].contains("+50.00%"), "{}", rows[0]);
        }

        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);
        let mut lower_latency = sample_metrics();
        lower_latency.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &lower_latency, 10.0);

        for format in [OutputFormat::Text, OutputFormat::Markdown] {
            let rendered = render_comparison_report(&comparison, format, 10.0);
            let rows = rows_for_metric(&rendered, "message_rtt.p50_us");
            assert_eq!(rows.len(), 1, "{rendered}");
            assert!(rows[0].contains("-50.00%"), "{}", rows[0]);
            assert!(!rows[0].contains("+50.00%"), "{}", rows[0]);
        }
    }

    #[cfg(feature = "compare")]
    #[test]
    fn improvement_pct_change_keeps_its_sign() {
        let baseline = sample_metrics();
        let mut faster = sample_metrics();
        faster.throughput_msg_per_sec = 150.0;
        let comparison = compare_metrics(&baseline, &faster, 10.0);
        let improvement = comparison
            .improvements
            .iter()
            .find(|item| item.metric == "throughput_msg_per_sec")
            .unwrap();
        assert_eq!(improvement.pct_change, 50.0);

        let mut baseline = sample_metrics();
        baseline.message_rtt = latency(1_000);
        let mut lower_latency = sample_metrics();
        lower_latency.message_rtt = latency(500);
        let comparison = compare_metrics(&baseline, &lower_latency, 10.0);
        let improvement = comparison
            .improvements
            .iter()
            .find(|item| item.metric == "message_rtt.p50_us")
            .unwrap();
        assert_eq!(improvement.pct_change, -50.0);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn comparison_json_carries_schema_version() {
        let comparison = compare_metrics(&sample_metrics(), &sample_metrics(), 10.0);
        let rendered = render_comparison_report(&comparison, OutputFormat::Json, 10.0);
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("comparison JSON must parse");
        assert_eq!(parsed["schema_version"], "1.1.0");
    }

    fn report_config(format: &str) -> LoadTestConfig {
        use clap::Parser;

        let cli =
            crate::cli::Cli::parse_from(["wsblast", "ws://127.0.0.1:9001", "--format", format]);
        LoadTestConfig::from_cli(cli).expect("valid config")
    }

    #[test]
    fn capture_totals_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown", "json"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("apture"), "{format}: {rendered}");
        }

        metrics.capture = Some(CaptureSummary {
            path: "run.jsonl".into(),
            frames_written: 120,
            frames_dropped: 3,
            limit: 100_000,
        });
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(
            text.contains("Capture: 120 frames written, 3 dropped (run.jsonl)"),
            "{text}"
        );
        let markdown = render_report(&report_config("markdown"), &metrics, &slo);
        assert!(
            markdown.contains("- **Capture:** 120 frames written, 3 dropped (`run.jsonl`)"),
            "{markdown}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_report(&report_config("json"), &metrics, &slo)).unwrap();
        assert_eq!(json["metrics"]["capture"]["frames_dropped"], 3);

        let parsed: AggregatedMetrics =
            serde_json::from_value(json["metrics"].clone()).expect("report parses");
        assert_eq!(parsed.capture, metrics.capture);
    }

    #[test]
    fn chaos_counts_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown", "json", "html"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("haos"), "{format}");
        }

        metrics.chaos = Some(ChaosSummary {
            seed: 42,
            delayed: 7,
            dropped: 3,
            corrupted: 2,
            closes: 1,
        });
        let line = "7 delayed, 3 dropped, 2 corrupted, 1 closes (seed 42; not counted as errors)";
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(text.contains(&format!("Chaos: {line}")), "{text}");
        let markdown = render_report(&report_config("markdown"), &metrics, &slo);
        assert!(
            markdown.contains(&format!("- **Chaos:** {line}")),
            "{markdown}"
        );
        let html = render_report(&report_config("html"), &metrics, &slo);
        assert!(html.contains(line), "html");
        let json: serde_json::Value =
            serde_json::from_str(&render_report(&report_config("json"), &metrics, &slo)).unwrap();
        assert_eq!(json["metrics"]["chaos"]["dropped"], 3);
        assert_eq!(json["metrics"]["chaos"]["seed"], 42);

        let parsed: AggregatedMetrics =
            serde_json::from_value(json["metrics"].clone()).expect("report parses");
        assert_eq!(parsed.chaos, metrics.chaos);
    }

    #[test]
    fn a_distributed_report_names_the_agents_whose_generator_failed() {
        use crate::metrics::{AgentGenerator, GeneratorLag, OpenFileLimit};
        let lag = |p99_us: u64, saturated: bool| GeneratorLag {
            probe_interval_us: 10_000,
            samples: 300,
            p50_us: 800,
            p99_us,
            max_us: p99_us * 2,
            threshold_us: 20_000,
            intervals: 3,
            intervals_over_threshold: u64::from(saturated),
            saturated,
        };
        let files = |soft_limit: u64, needed: u64| OpenFileLimit {
            soft_limit,
            needed,
            sufficient: needed <= soft_limit,
        };
        let mut metrics = sample_metrics();
        // The run's lag is agent b's, the highest p99, and it is flagged
        // because agent c's generator was saturated.
        metrics.generator_lag = Some(GeneratorLag {
            saturated: true,
            ..lag(30_000, false)
        });
        metrics.open_file_limit = Some(files(100, 120));
        metrics.distributed = Some(DistributedSummary {
            agents: vec!["a:1".into(), "b:1".into(), "c:1".into()],
            lost_agents: Vec::new(),
            generators: vec![
                AgentGenerator {
                    agent: "a:1".into(),
                    generator_lag: Some(lag(900, false)),
                    open_file_limit: Some(files(100, 120)),
                },
                AgentGenerator {
                    agent: "b:1".into(),
                    generator_lag: Some(lag(30_000, false)),
                    open_file_limit: Some(files(1_000, 120)),
                },
                AgentGenerator {
                    agent: "c:1".into(),
                    generator_lag: Some(lag(19_000, true)),
                    open_file_limit: Some(files(1_000, 120)),
                },
            ],
            generator_lag_agent: Some("b:1".into()),
            open_file_limit_agent: Some("a:1".into()),
        });
        let line = "lag p99 30.00 ms, max 60.00 ms over 300 wake-ups on agent b:1, the highest of 3 agents \
                    (threshold 20.00 ms; SATURATED on c:1: latency includes client-side delay); \
                    open file limit 100 for 120 needed on agent a:1, the tightest of 3 agents (too low on a:1)";
        assert_eq!(generator_label(&metrics).as_deref(), Some(line));
        let slo = evaluate_slos(
            &SloThresholds::new().with_max_generator_lag(Duration::from_millis(25)),
            &metrics,
        );
        assert!(!slo.passed, "agent b's p99 is over the limit");
        for format in ["text", "markdown", "html"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(rendered.contains(line), "{format}: {rendered}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&render_report(&report_config("json"), &metrics, &slo)).unwrap();
        let distributed = &json["metrics"]["distributed"];
        assert_eq!(distributed["generator_lag_agent"], "b:1");
        assert_eq!(distributed["open_file_limit_agent"], "a:1");
        assert_eq!(
            distributed["generators"][2]["generator_lag"]["saturated"],
            true
        );
        let parsed: AggregatedMetrics =
            serde_json::from_value(json["metrics"].clone()).expect("report parses");
        assert_eq!(parsed.distributed, metrics.distributed);
    }

    #[test]
    fn generator_health_is_reported_and_gated() {
        let mut metrics = sample_metrics();
        let slo = SloThresholds::new().with_max_generator_lag(Duration::from_millis(5));
        let evaluation = evaluate_slos(&slo, &metrics);
        assert!(!evaluation.passed);
        assert_eq!(evaluation.checks[0].name, "Generator lag p99");
        assert_eq!(evaluation.checks[0].actual, "no lag samples");
        assert!(generator_label(&metrics).is_none());

        metrics.generator_lag = Some(crate::metrics::GeneratorLag {
            probe_interval_us: 10_000,
            samples: 500,
            p50_us: 900,
            p99_us: 4_000,
            max_us: 7_500,
            threshold_us: 20_000,
            intervals: 5,
            intervals_over_threshold: 0,
            saturated: false,
        });
        metrics.open_file_limit = Some(crate::metrics::OpenFileLimit {
            soft_limit: 1024,
            needed: 2064,
            sufficient: false,
        });
        assert!(evaluate_slos(&slo, &metrics).passed);
        let line = "lag p99 4.00 ms, max 7.50 ms over 500 wake-ups (threshold 20.00 ms; ok); open file limit 1024 for 2064 needed (too low)";
        let evaluation = evaluate_slos(&SloThresholds::default(), &metrics);
        let text = render_report(&report_config("text"), &metrics, &evaluation);
        assert!(text.contains(&format!("Generator: {line}")), "{text}");
        let markdown = render_report(&report_config("markdown"), &metrics, &evaluation);
        assert!(
            markdown.contains(&format!("- **Generator:** {line}")),
            "{markdown}"
        );
        let json: serde_json::Value = serde_json::from_str(&render_report(
            &report_config("json"),
            &metrics,
            &evaluation,
        ))
        .unwrap();
        assert_eq!(json["metrics"]["generator_lag"]["p99_us"], 4_000);
        assert_eq!(json["metrics"]["open_file_limit"]["sufficient"], false);
        let parsed: AggregatedMetrics =
            serde_json::from_value(json["metrics"].clone()).expect("report parses");
        assert_eq!(parsed.generator_lag, metrics.generator_lag);

        metrics.generator_lag.as_mut().unwrap().p99_us = 6_000;
        assert!(!evaluate_slos(&slo, &metrics).passed);
    }

    #[test]
    fn distributed_runs_list_their_agents_and_lost_agents() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown", "json"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Agents"), "{format}: {rendered}");
            assert!(!rendered.contains("distributed"), "{format}");
        }
        metrics.distributed = Some(DistributedSummary {
            agents: vec![
                "10.0.0.1:7700".into(),
                "10.0.0.2:7700".into(),
                "10.0.0.3:7700".into(),
            ],
            lost_agents: vec!["10.0.0.2:7700".into()],
            ..Default::default()
        });
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(text.contains("3 (lost: 10.0.0.2:7700)"), "{text}");
        let markdown = render_report(&report_config("markdown"), &metrics, &slo);
        assert!(
            markdown.contains("- **Agents:** 3 (lost: `10.0.0.2:7700`)"),
            "{markdown}"
        );
        let json = render_report(&report_config("json"), &metrics, &slo);
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            json["metrics"]["distributed"]["lost_agents"][0],
            "10.0.0.2:7700"
        );
        assert_eq!(
            json["metrics"]["distributed"]["agents"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let parsed: AggregatedMetrics = serde_json::from_value(json["metrics"].clone()).unwrap();
        assert_eq!(parsed.distributed, metrics.distributed);

        metrics.distributed.as_mut().unwrap().lost_agents.clear();
        let text = render_report(&report_config("text"), &metrics, &slo);
        assert!(!text.contains("lost"), "{text}");
    }

    #[test]
    fn unsolicited_frames_are_reported_only_when_seen() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Unsolicited"), "{format}: {rendered}");
        }

        metrics.total_unsolicited_frames = 42;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Unsolicited"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains("42"), "{format}: {line}");
        }
    }

    #[test]
    fn not_started_connections_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Not Started"), "{format}: {rendered}");
        }

        metrics.total_connections_established = 3;
        metrics.total_connections_not_started = 7;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Not Started"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains('7'), "{format}: {line}");
        }
    }

    #[test]
    fn paced_runs_gate_on_response_time_and_show_it() {
        let slo = SloThresholds::new().with_max_p99(Duration::from_millis(1));
        let mut metrics = sample_metrics();

        let unpaced = evaluate_slos(&slo, &metrics);
        assert_eq!(unpaced.checks[0].name, "Latency p99");
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &unpaced);
            assert!(!rendered.contains("Response Time"), "{format}: {rendered}");
        }

        // RTT is within the 1 ms budget but response time is not.
        metrics.response_time = latency(5_000);
        let paced = evaluate_slos(&slo, &metrics);
        assert_eq!(paced.checks[0].name, "Response time p99");
        assert!(!paced.passed);
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &paced);
            assert!(rendered.contains("Response Time"), "{format}: {rendered}");
        }
    }

    #[cfg(feature = "compare")]
    #[test]
    fn response_time_is_compared_only_when_both_reports_have_it() {
        let baseline = sample_metrics();
        let mut current = sample_metrics();
        current.response_time = latency(9_000);
        let comparison = compare_metrics(&baseline, &current, 5.0);
        assert!(
            comparison
                .regressions
                .iter()
                .all(|item| !item.metric.starts_with("response_time.")),
            "{:?}",
            comparison.regressions
        );

        let mut baseline = current.clone();
        baseline.response_time = latency(1_000);
        let comparison = compare_metrics(&baseline, &current, 5.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "response_time.p99_us"),
            "{:?}",
            comparison.regressions
        );
    }

    /// `sample_metrics` with a 10-point series at 100 msg/s and a 250 us
    /// p99, where `stall` makes interval 5 the one whose late replies came
    /// in with a p99 of `stall` microseconds.
    #[cfg(feature = "compare")]
    fn metrics_with_series(stall: Option<u64>) -> AggregatedMetrics {
        use crate::timeseries::{IntervalLatency, TimeSeries, TimeSeriesPoint};
        let points = (0..10)
            .map(|i| TimeSeriesPoint {
                start_secs: f64::from(i),
                duration_secs: 1.0,
                messages_sent: 100,
                messages_recv: 100,
                throughput_msg_per_sec: 100.0,
                latency: IntervalLatency {
                    count: 100,
                    p99_us: match stall {
                        Some(stall) if i == 5 => stall,
                        _ => 250,
                    },
                    ..IntervalLatency::default()
                },
                ..TimeSeriesPoint::default()
            })
            .collect();
        let mut metrics = sample_metrics();
        metrics.time_series = TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        };
        metrics
    }

    #[cfg(feature = "compare")]
    fn changed_metrics(comparison: &BaselineComparison) -> Vec<&str> {
        comparison
            .regressions
            .iter()
            .map(|item| item.metric.as_str())
            .chain(
                comparison
                    .improvements
                    .iter()
                    .map(|item| item.metric.as_str()),
            )
            .collect()
    }

    #[cfg(feature = "compare")]
    #[test]
    fn series_metrics_are_compared_only_when_both_reports_have_a_series() {
        let stalled = metrics_with_series(Some(2_000_000));
        for (baseline, current) in [
            (sample_metrics(), stalled.clone()),
            (stalled.clone(), sample_metrics()),
        ] {
            let comparison = compare_metrics(&baseline, &current, 5.0);
            assert!(!comparison.series_compared);
            assert!(
                changed_metrics(&comparison).is_empty(),
                "{:?}",
                changed_metrics(&comparison)
            );
        }

        let comparison = compare_metrics(&metrics_with_series(None), &stalled, 5.0);
        assert!(comparison.series_compared);
        let regressed: Vec<&str> = comparison
            .regressions
            .iter()
            .map(|item| item.metric.as_str())
            .collect();
        assert_eq!(regressed, ["series.p99_worst_us"]);
        assert!(comparison.improvements.is_empty());
        let worst = &comparison.regressions[0];
        assert_eq!((worst.baseline, worst.current), (250.0, 2_000_000.0));
    }

    #[cfg(feature = "compare")]
    #[test]
    fn series_metrics_follow_their_direction() {
        let baseline = metrics_with_series(None);
        let mut current = metrics_with_series(None);
        for point in &mut current.time_series.points {
            point.throughput_msg_per_sec = 150.0;
            point.errors = 1;
        }
        let comparison = compare_metrics(&baseline, &current, 5.0);
        let improved: Vec<&str> = comparison
            .improvements
            .iter()
            .map(|item| item.metric.as_str())
            .collect();
        assert_eq!(improved, ["series.throughput_mean"]);
        let regressed: Vec<&str> = comparison
            .regressions
            .iter()
            .map(|item| item.metric.as_str())
            .collect();
        assert_eq!(
            regressed,
            ["series.error_intervals", "series.max_interval_errors"]
        );

        // A series with one interval has no throughput CV; the other
        // report's CV is not compared against a missing value.
        let mut single = metrics_with_series(None);
        single.time_series.points.truncate(1);
        let mut sawtooth = metrics_with_series(None);
        for (i, point) in sawtooth.time_series.points.iter_mut().enumerate() {
            point.throughput_msg_per_sec = if i % 2 == 0 { 50.0 } else { 150.0 };
        }
        let comparison = compare_metrics(&single, &sawtooth, 5.0);
        assert!(
            !changed_metrics(&comparison).contains(&"series.throughput_cv"),
            "{:?}",
            changed_metrics(&comparison)
        );
        let comparison = compare_metrics(&metrics_with_series(None), &sawtooth, 5.0);
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "series.throughput_cv")
        );
    }

    #[cfg(feature = "compare")]
    #[test]
    fn the_window_restricts_only_series_metrics() {
        let baseline = metrics_with_series(None);
        let current = metrics_with_series(Some(2_000_000));
        let window = CompareWindow::parse("6s..").unwrap();
        let comparison = compare_metrics_in(&baseline, &current, 5.0, Some(&window));
        assert!(comparison.series_compared);
        assert!(
            changed_metrics(&comparison).is_empty(),
            "{:?}",
            changed_metrics(&comparison)
        );

        let mut slower = current.clone();
        slower.message_rtt = latency(1_000);
        let comparison = compare_metrics_in(&baseline, &slower, 5.0, Some(&window));
        assert!(
            comparison
                .regressions
                .iter()
                .any(|item| item.metric == "message_rtt.p99_us")
        );
    }

    #[cfg(feature = "compare")]
    #[test]
    fn comparison_schema_version_moves_to_1_2_only_with_series_or_window() {
        let version = |comparison: &BaselineComparison| {
            let rendered = render_comparison_report(comparison, OutputFormat::Json, 10.0);
            let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            (
                parsed["schema_version"].clone(),
                parsed.get("window").cloned(),
            )
        };
        let legacy = compare_metrics(&sample_metrics(), &sample_metrics(), 10.0);
        assert_eq!(version(&legacy), (serde_json::json!("1.1.0"), None));

        let series = metrics_with_series(None);
        let both = compare_metrics(&series, &series, 10.0);
        assert_eq!(version(&both), (serde_json::json!("1.2.0"), None));

        let window = CompareWindow::parse("2s..8s").unwrap();
        let windowed = compare_metrics_in(&series, &series, 10.0, Some(&window));
        assert_eq!(
            version(&windowed),
            (
                serde_json::json!("1.2.0"),
                Some(serde_json::json!({ "start_secs": 2.0, "end_secs": 8.0 }))
            )
        );
        let open = CompareWindow::parse("..8s").unwrap();
        let open_start = compare_metrics_in(&series, &series, 10.0, Some(&open));
        assert_eq!(
            version(&open_start).1,
            Some(serde_json::json!({ "start_secs": 0.0, "end_secs": 8.0 }))
        );

        for format in [OutputFormat::Text, OutputFormat::Markdown] {
            let rendered = strip_ansi(&render_comparison_report(&windowed, format, 10.0));
            assert!(rendered.contains("Window:"), "{rendered}");
            assert!(rendered.contains("2s..8s"), "{rendered}");
            let rendered = render_comparison_report(&both, format, 10.0);
            assert!(!rendered.contains("Window"), "{rendered}");
        }
    }

    #[test]
    fn time_series_is_summarised_in_at_most_ten_rows() {
        use crate::timeseries::{TimeSeries, TimeSeriesPoint};
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Time Window"), "{format}: {rendered}");
        }

        let points: Vec<TimeSeriesPoint> = (0..25)
            .map(|i| TimeSeriesPoint {
                start_secs: f64::from(i),
                duration_secs: 1.0,
                messages_sent: 100,
                latency: crate::timeseries::IntervalLatency {
                    count: 100,
                    p99_us: if i == 12 { 900_000 } else { 1_000 },
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        metrics.time_series = TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        };

        let rendered = render_report(&report_config("markdown"), &metrics, &slo);
        let rows: Vec<&str> = rendered
            .lines()
            .filter(|line| line.contains("s to "))
            .collect();
        assert_eq!(rows.len(), 9, "{rendered}");
        assert!(
            rows[0].starts_with("| 0s to 3s | 100.0 | 0 |"),
            "{rendered}"
        );
        assert!(rows[4].contains("900.00 ms"), "{rendered}");
        assert!(rows[8].starts_with("| 24s to 25s |"), "{rendered}");
        assert!(rendered.contains("Worst RTT p99"), "{rendered}");
    }

    #[test]
    fn interrupted_connections_are_reported_only_when_present() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut metrics = sample_metrics();

        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            assert!(!rendered.contains("Interrupted"), "{format}: {rendered}");
        }

        metrics.total_connections_established = 6;
        metrics.total_connections_interrupted = 4;
        for format in ["text", "markdown"] {
            let rendered = render_report(&report_config(format), &metrics, &slo);
            let line = rendered
                .lines()
                .find(|line| line.contains("Interrupted"))
                .unwrap_or_else(|| panic!("{format}: {rendered}"));
            assert!(line.contains('4'), "{format}: {line}");
        }
    }

    #[test]
    fn scenario_reports_list_every_phase_target() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let phases = [
            PhaseTarget {
                phase: "warmup".to_string(),
                target_url: url::Url::parse("ws://first.test/ws").expect("url"),
            },
            PhaseTarget {
                phase: "spike".to_string(),
                target_url: url::Url::parse("wss://second.test/ws").expect("url"),
            },
        ];

        for format in ["text", "markdown"] {
            let rendered = render_scenario_report(
                &report_config(format),
                &sample_metrics(),
                &slo,
                &phases,
                &[],
            );
            for phase in &phases {
                let line = rendered
                    .lines()
                    .find(|line| line.contains(phase.target_url.as_str()))
                    .unwrap_or_else(|| panic!("{format}: {rendered}"));
                assert!(line.contains(&phase.phase), "{format}: {line}");
            }
        }

        let rendered = render_scenario_report(
            &report_config("json"),
            &sample_metrics(),
            &slo,
            &phases,
            &[],
        );
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(
            parsed["phase_targets"],
            serde_json::json!([
                {"phase": "warmup", "target_url": "ws://first.test/ws"},
                {"phase": "spike", "target_url": "wss://second.test/ws"},
            ])
        );

        let single = render_report(&report_config("json"), &sample_metrics(), &slo);
        let parsed: serde_json::Value = serde_json::from_str(&single).expect("json");
        assert!(parsed.get("phase_targets").is_none(), "{single}");
    }

    /// The scenario reports of a live run, compared byte for byte with the
    /// files the renderers wrote before reports were rendered from a view
    /// (`WSBLAST_BLESS=1` rewrites them). tests/report_render.rs covers the
    /// single-run reports.
    #[test]
    fn live_scenario_reports_are_unchanged() {
        let mut metrics = sample_metrics();
        metrics
            .error_breakdown
            .insert(crate::error::ErrorCategory::Timeout, 3);
        let slo = evaluate_slos(
            &SloThresholds::new().with_max_p99(Duration::from_millis(1)),
            &metrics,
        );
        let phases = [
            PhaseTarget {
                phase: "warmup".to_string(),
                target_url: url::Url::parse("ws://first.test/ws").expect("url"),
            },
            PhaseTarget {
                phase: "spike".to_string(),
                target_url: url::Url::parse("wss://second.test/ws?a=1&b=2").expect("url"),
            },
        ];
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live_report");
        for (format, extension) in [("text", "txt"), ("json", "json"), ("markdown", "md")] {
            let rendered =
                render_scenario_report(&report_config(format), &metrics, &slo, &phases, &[]);
            let rendered: String = strip_ansi(&rendered);
            let path = dir.join(format!("scenario.{extension}"));
            if std::env::var_os("WSBLAST_BLESS").is_some() {
                std::fs::write(&path, &rendered).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .replace("\r\n", "\n");
            assert_eq!(expected, rendered, "{}", path.display());
        }
    }

    fn phase_report(name: &str, slo: &SloThresholds) -> PhaseReport {
        let metrics = sample_metrics();
        PhaseReport {
            name: name.to_string(),
            target_url: url::Url::parse("ws://phase.test/ws").expect("url"),
            connections: 10,
            stages: Vec::new(),
            slo: evaluate_slos(slo, &metrics),
            metrics,
        }
    }

    #[cfg(feature = "scenario")]
    #[test]
    fn phase_checks_are_prefixed_and_gate_the_run() {
        let metrics = sample_metrics();
        let overall = evaluate_slos(&SloThresholds::new().with_max_error_rate(0.5), &metrics);
        let warmup = evaluate_slos(&SloThresholds::new(), &metrics);
        let spike = evaluate_slos(
            &SloThresholds::new().with_max_p99(Duration::from_micros(100)),
            &metrics,
        );
        let combined =
            scenario_slo_evaluation(overall.clone(), [("warmup", &warmup), ("spike", &spike)]);
        assert!(!combined.passed);
        let names: Vec<&str> = combined.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Error Rate", "spike: Latency p99"]);
        assert!(combined.checks[0].passed && !combined.checks[1].passed);

        let passing = scenario_slo_evaluation(overall, [("warmup", &warmup)]);
        assert!(passing.passed);
        assert_eq!(passing.checks.len(), 1);
    }

    #[test]
    fn phase_tables_appear_only_when_a_phase_sets_an_slo() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let targets = [PhaseTarget {
            phase: "warmup".to_string(),
            target_url: url::Url::parse("ws://phase.test/ws").expect("url"),
        }];
        let plain = [
            phase_report("warmup", &SloThresholds::new()),
            phase_report("spike", &SloThresholds::new()),
        ];
        for format in ["text", "markdown"] {
            let with_phases = render_scenario_report(
                &report_config(format),
                &sample_metrics(),
                &slo,
                &targets,
                &plain,
            );
            let without = render_scenario_report(
                &report_config(format),
                &sample_metrics(),
                &slo,
                &targets,
                &[],
            );
            assert_eq!(strip_ansi(&with_phases), strip_ansi(&without), "{format}");
        }

        let gated = [
            phase_report("warmup", &SloThresholds::new()),
            phase_report(
                "spike",
                &SloThresholds::new().with_max_p99(Duration::from_micros(100)),
            ),
        ];
        let text = strip_ansi(&render_scenario_report(
            &report_config("text"),
            &sample_metrics(),
            &slo,
            &targets,
            &gated,
        ));
        let row = |name: &str| {
            text.lines()
                .find(|line| line.contains(&format!(" {name} ")) && line.contains("phase.test"))
                .unwrap_or_else(|| panic!("{text}"))
                .to_string()
        };
        assert!(row("warmup").contains("10 connections"), "{text}");
        assert!(row("warmup").contains(" - "), "{text}");
        assert!(row("spike").contains("FAILED"), "{text}");
        assert!(row("spike").contains("250 µs (RTT)"), "{text}");
        assert!(row("spike").contains("100.0"), "{text}");

        let markdown = render_scenario_report(
            &report_config("markdown"),
            &sample_metrics(),
            &slo,
            &targets,
            &gated,
        );
        assert!(markdown.contains("### Phases"), "{markdown}");
        assert!(
            markdown.contains(
                "| `spike` | `ws://phase.test/ws` | 10 connections | 100.0 | 0.00% | 250 µs (RTT) | **❌ FAIL** |"
            ),
            "{markdown}"
        );
        assert!(
            markdown.contains("| `warmup` | `ws://phase.test/ws` | 10 connections | 100.0 | 0.00% | 250 µs (RTT) | **-** |"),
            "{markdown}"
        );
    }

    #[test]
    fn staged_phases_show_their_stages_instead_of_a_connection_count() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let mut staged = phase_report("ramp", &SloThresholds::new());
        staged.stages = vec![
            StageReport {
                duration: Duration::from_secs(30),
                target: 100,
            },
            StageReport {
                duration: Duration::from_millis(1_500),
                target: 0,
            },
        ];
        let phases = [phase_report("warmup", &SloThresholds::new()), staged];
        assert_eq!(phases[1].load_label(), "30s:100, 1.5s:0");

        // Stages alone are enough to show the phase table.
        let text = strip_ansi(&render_scenario_report(
            &report_config("text"),
            &sample_metrics(),
            &slo,
            &[],
            &phases,
        ));
        let row = text
            .lines()
            .find(|line| line.contains(" ramp "))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(row.contains("30s:100, 1.5s:0"), "{row}");
        assert!(text.contains("10 connections"), "{text}");
        let markdown = render_scenario_report(
            &report_config("markdown"),
            &sample_metrics(),
            &slo,
            &[],
            &phases,
        );
        assert!(
            markdown.contains("| `ramp` | `ws://phase.test/ws` | 30s:100, 1.5s:0 |"),
            "{markdown}"
        );

        let json = render_scenario_report(
            &report_config("json"),
            &sample_metrics(),
            &slo,
            &[],
            &phases,
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["phases"][0].get("stages").is_none(), "{json}");
        assert_eq!(
            value["phases"][1]["stages"],
            serde_json::json!([
                {"duration": {"secs": 30, "nanos": 0}, "target": 100},
                {"duration": {"secs": 1, "nanos": 500_000_000}, "target": 0},
            ])
        );
        assert_eq!(SavedReport::parse(&json).unwrap().phases, phases);
    }

    #[test]
    fn json_reports_carry_every_phase_and_read_back() {
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let phases = [
            phase_report("warmup", &SloThresholds::new()),
            phase_report("spike", &SloThresholds::new().with_min_throughput(1e6)),
        ];
        let json = render_scenario_report(
            &report_config("json"),
            &sample_metrics(),
            &slo,
            &[],
            &phases,
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["phases"][0]["name"], "warmup");
        assert_eq!(value["phases"][1]["slo"]["passed"], false);
        assert_eq!(value["phases"][1]["metrics"]["total_messages_sent"], 100);
        assert_eq!(value["phases"][1]["connections"], 10);

        let saved = SavedReport::parse(&json).unwrap();
        assert_eq!(saved.phases, phases);

        let single = render_report(&report_config("json"), &sample_metrics(), &slo);
        let value: serde_json::Value = serde_json::from_str(&single).unwrap();
        assert!(value.get("phases").is_none(), "{single}");
        assert!(SavedReport::parse(&single).unwrap().phases.is_empty());
    }

    #[test]
    fn json_reports_record_the_rate_and_stages_for_rendering_later() {
        use clap::Parser;

        let cli = crate::cli::Cli::parse_from([
            "wsblast",
            "ws://127.0.0.1:9001",
            "--stages",
            "3s:30,4s:30,2s:0",
            "-r",
            "20",
            "--format",
            "json",
        ]);
        let config = LoadTestConfig::from_cli(cli).expect("valid config");
        let slo = evaluate_slos(&SloThresholds::default(), &sample_metrics());
        let json = render_report(&config, &sample_metrics(), &slo);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["rate_per_conn"], 20);
        assert_eq!(value["stages"].as_array().map(Vec::len), Some(3));
        assert_eq!(value["stages"][0]["target"], 30);

        let saved = SavedReport::parse(&json).unwrap();
        let view = saved.view(None);
        assert_eq!(view.rate_per_conn, Some(20));
        assert_eq!(view.stages_label().as_deref(), Some("3s:30, 4s:30, 2s:0"));
        let html = crate::html::render_html_report(&view);
        assert!(html.contains("3s:30, 4s:30, 2s:0"), "{html}");
        assert!(html.contains("20 msg/s per connection"), "{html}");

        // A constant run records its rate but no stages; a scenario neither.
        let constant = render_report(&report_config("json"), &sample_metrics(), &slo);
        let value: serde_json::Value = serde_json::from_str(&constant).unwrap();
        assert_eq!(value["rate_per_conn"], 0);
        assert!(value.get("stages").is_none(), "{constant}");
        let scenario = render_scenario_report(
            &report_config("json"),
            &sample_metrics(),
            &slo,
            &[],
            &[phase_report("warmup", &SloThresholds::new())],
        );
        let value: serde_json::Value = serde_json::from_str(&scenario).unwrap();
        assert!(value.get("rate_per_conn").is_none(), "{scenario}");

        // Reports written before the fields existed still read back.
        let mut old: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = old.as_object_mut().unwrap();
        object.remove("rate_per_conn");
        object.remove("stages");
        let saved = SavedReport::parse(&old.to_string()).unwrap();
        assert_eq!(saved.view(None).rate_per_conn, None);
        assert!(saved.view(None).stages.is_empty());
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_unsolicited_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_unsolicited_frames");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_unsolicited_frames, 0);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_not_started_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_connections_not_started");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_connections_not_started, 0);
    }

    #[cfg(feature = "compare")]
    #[test]
    fn reports_without_interrupted_counter_still_parse() {
        let mut legacy = serde_json::to_value(sample_metrics()).expect("serialize");
        legacy
            .as_object_mut()
            .expect("metrics object")
            .remove("total_connections_interrupted");

        let parsed = parse_report_metrics(&legacy.to_string()).expect("legacy report must parse");
        assert_eq!(parsed.total_connections_interrupted, 0);
    }
}
