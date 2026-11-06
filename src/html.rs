//! The `--format html` report: one self-contained page with inline CSS and
//! inline SVG charts of the time series.
//!
//! The page needs no script, font, stylesheet or image from anywhere else,
//! so it renders offline and as a CI artifact. Every string that comes from
//! the report (target URLs, phase names, SLO check names and values) is
//! HTML-escaped, because a saved JSON report can hold any text.

use crate::report::{PercentileRow, ReportView, format_secs, format_us, latency_columns};
use crate::timeseries::TimeSeriesPoint;
use std::fmt::Write;

/// Escapes text for use in HTML content and in quoted attribute values.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --surface: #fcfcfb;
  --panel: #ffffff;
  --border: #e3e2de;
  --text: #0b0b0b;
  --text-2: #52514e;
  --muted: #7a7974;
  --grid: #ecebe7;
  --axis: #b9b8b2;
  --series: #2a78d6;
  --series-2: #eb6834;
  --hover: rgba(42, 120, 214, 0.10);
  --good: #0ca30c;
  --good-bg: #e9f7e9;
  --bad: #d03b3b;
  --bad-bg: #fbeaea;
  --neutral-bg: #f0efec;
}
@media (prefers-color-scheme: dark) {
  :root {
    --surface: #1a1a19;
    --panel: #222220;
    --border: #383835;
    --text: #f0efec;
    --text-2: #c3c2b7;
    --muted: #9a998f;
    --grid: #2e2e2b;
    --axis: #5a5953;
    --series: #3987e5;
    --series-2: #d95926;
    --hover: rgba(57, 135, 229, 0.16);
    --good-bg: #16301a;
    --bad-bg: #3a1b1b;
    --neutral-bg: #2b2b28;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--surface);
  color: var(--text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
}
main { max-width: 1000px; margin: 0 auto; padding: 24px 16px 48px; }
h1 { font-size: 1.5rem; margin: 0 0 4px; }
h2 { font-size: 1.15rem; margin: 32px 0 12px; }
h3 { font-size: 1rem; margin: 0 0 8px; }
h3.phase-checks { margin-top: 20px; }
.subtitle { color: var(--text-2); margin: 0 0 16px; }
.meta { display: grid; grid-template-columns: max-content 1fr; gap: 4px 16px; margin: 0; }
.meta dt { color: var(--text-2); }
.meta dd { margin: 0; overflow-wrap: anywhere; }
.meta ul { margin: 0; padding-left: 18px; }
code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.92em; }
.banner { border-radius: 8px; padding: 16px; margin-top: 24px; border: 1px solid var(--border); }
.banner.pass { background: var(--good-bg); border-color: var(--good); }
.banner.fail { background: var(--bad-bg); border-color: var(--bad); }
.banner.none { background: var(--neutral-bg); }
.banner .verdict { font-size: 1.15rem; font-weight: 700; margin: 0; }
.banner table { margin-top: 12px; }
.ok { color: var(--good); font-weight: 600; }
.ko { color: var(--bad); font-weight: 600; }
.kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 12px; margin-top: 24px; }
.kpi { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 12px 16px; }
.kpi .label { color: var(--text-2); font-size: 0.85rem; margin: 0; }
.kpi .value { font-size: 1.5rem; font-weight: 650; margin: 2px 0 0; font-variant-numeric: tabular-nums; }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--border); white-space: nowrap; }
th { color: var(--text-2); font-weight: 600; }
td.num, th.num { text-align: right; }
.charts { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 440px), 1fr)); gap: 16px; }
figure { margin: 0; background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 12px 12px 8px; min-width: 0; }
figcaption { color: var(--text-2); font-size: 0.85rem; margin-top: 4px; }
svg.chart { display: block; width: 100%; height: auto; }
svg.chart text { fill: var(--muted); font-size: 11px; font-family: inherit; }
svg.chart .grid { stroke: var(--grid); stroke-width: 1; }
svg.chart .axis { stroke: var(--axis); stroke-width: 1; }
svg.chart .line { fill: none; stroke: var(--series); stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
svg.chart .dot { fill: var(--series); stroke: var(--panel); stroke-width: 2; }
svg.chart .bar { fill: var(--series-2); }
svg.chart .hit { fill: transparent; }
svg.chart .hit:hover { fill: var(--hover); }
svg.chart .empty { fill: var(--text-2); font-size: 13px; }
details { margin-top: 6px; font-size: 0.85rem; }
details table { margin-top: 6px; }
details .scroll { max-height: 320px; overflow-y: auto; }
summary { cursor: pointer; color: var(--text-2); }
.note { color: var(--text-2); }
footer { margin-top: 40px; color: var(--muted); font-size: 0.85rem; }
"#;

/// Renders the HTML report.
pub(crate) fn render_html_report(view: &ReportView<'_>) -> String {
    let mut html = String::with_capacity(64 * 1024);
    let title_target = view
        .phase_targets
        .first()
        .map_or(view.target_url, |phase| phase.target_url.as_str());
    let _ = write!(
        html,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <title>wsblast report: {}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n",
        escape(title_target)
    );

    render_header(&mut html, view);
    render_slo_banner(&mut html, view);
    render_key_numbers(&mut html, view);
    render_phases(&mut html, view);
    render_latency_table(&mut html, view);
    render_charts(&mut html, view);
    render_errors(&mut html, view);
    render_traffic(&mut html, view);

    let _ = write!(
        html,
        "<footer>Generated by wsblast {}. The charts plot every point of the run's time series; \
         see METHODOLOGY.md section 6.3 for how each interval is measured.</footer>\n\
         </main>\n</body>\n</html>\n",
        env!("CARGO_PKG_VERSION")
    );
    html
}

fn render_header(html: &mut String, view: &ReportView<'_>) {
    html.push_str("<header>\n<h1>wsblast WebSocket benchmark report</h1>\n");
    html.push_str("<dl class=\"meta\">\n");
    if view.phase_targets.is_empty() {
        let _ = writeln!(
            html,
            "<dt>Target</dt><dd><code>{}</code></dd>",
            escape(view.target_url)
        );
    } else {
        html.push_str("<dt>Phases</dt><dd><ul>");
        for phase in view.phase_targets {
            let _ = write!(
                html,
                "<li>{}: <code>{}</code></li>",
                escape(&phase.phase),
                escape(phase.target_url.as_str())
            );
        }
        html.push_str("</ul></dd>\n");
    }
    let _ = writeln!(html, "<dt>Mode</dt><dd>{}</dd>", escape(&view.mode));
    if let Some((label, time)) = &view.timestamp {
        let _ = writeln!(
            html,
            "<dt>{}</dt><dd>{}</dd>",
            escape(label),
            escape(&readable_timestamp(time))
        );
    }
    let _ = writeln!(
        html,
        "<dt>Duration</dt><dd>{}</dd>",
        escape(&format!("{:.2?}", view.metrics.elapsed))
    );
    let connections_label = if view.phase_targets.is_empty() {
        "Connections"
    } else {
        "Connections (all phases)"
    };
    let _ = writeln!(
        html,
        "<dt>{connections_label}</dt><dd>{}</dd>",
        view.connections
    );
    if let Some(stages) = &view.stages_label() {
        let _ = writeln!(
            html,
            "<dt>Load stages</dt><dd><code>{}</code></dd>",
            escape(stages)
        );
    }
    let rate = match view.rate_per_conn {
        Some(0) => "Unthrottled".to_string(),
        Some(rate) => format!("{rate} msg/s per connection"),
        None => "Not recorded in the JSON report".to_string(),
    };
    let _ = writeln!(html, "<dt>Rate</dt><dd>{}</dd>", escape(&rate));
    if let Some(distributed) = &view.metrics.distributed {
        let _ = writeln!(
            html,
            "<dt>Agents</dt><dd>{}</dd>",
            crate::report::agents_label(distributed, escape)
        );
    }
    html.push_str("</dl>\n</header>\n");
}

/// `2026-10-25T14:03:07.123Z` as `2026-10-25 14:03:07 UTC`; other text is
/// returned unchanged.
fn readable_timestamp(time: &str) -> String {
    match (time.get(..10), time.get(11..19)) {
        (Some(date), Some(clock))
            if time.ends_with('Z') && time.as_bytes().get(10) == Some(&b'T') =>
        {
            format!("{date} {clock} UTC")
        }
        _ => time.to_string(),
    }
}

fn render_slo_banner(html: &mut String, view: &ReportView<'_>) {
    let slo = view.slo;
    let total = slo.checks.len();
    let failed = slo.checks.iter().filter(|check| !check.passed).count();
    let (class, verdict) = if total == 0 {
        ("none", "No SLO thresholds were set".to_string())
    } else if slo.passed && failed == 0 {
        (
            "pass",
            format!(
                "PASS: {} SLO check{} passed",
                if total == 1 { "the" } else { "all" },
                if total == 1 { "" } else { "s" }
            ),
        )
    } else {
        (
            "fail",
            format!("FAIL: {failed} of {total} SLO checks failed"),
        )
    };
    let icon = match class {
        "pass" => "\u{2713} ",
        "fail" => "\u{2717} ",
        _ => "",
    };
    let _ = write!(
        html,
        "<section class=\"banner {class}\" aria-label=\"SLO gate\">\n<p class=\"verdict\">{icon}{}</p>\n",
        escape(&verdict)
    );
    if total > 0 {
        html.push_str(
            "<div class=\"scroll\"><table>\n<thead><tr><th scope=\"col\">Objective</th>\
             <th scope=\"col\">Target</th><th scope=\"col\">Actual</th>\
             <th scope=\"col\">Result</th></tr></thead>\n<tbody>\n",
        );
        for check in &slo.checks {
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                escape(&check.name),
                escape(&check.target),
                escape(&check.actual),
                check_result(check.passed)
            );
        }
        html.push_str("</tbody></table></div>\n");
    }
    html.push_str("</section>\n");
}

/// The latency the SLO gate reads: response time for paced runs, RTT
/// otherwise, as [`evaluate_slos`](crate::evaluate_slos) chooses it.
fn gating_latency<'a>(view: &ReportView<'a>) -> (&'static str, &'a crate::LatencyStats) {
    if view.metrics.response_time.count > 0 {
        ("Response time", &view.metrics.response_time)
    } else {
        ("RTT", &view.metrics.message_rtt)
    }
}

fn render_key_numbers(html: &mut String, view: &ReportView<'_>) {
    let metrics = view.metrics;
    let (basis, stats) = gating_latency(view);
    let latency = |value: u64| {
        if stats.count == 0 {
            "no samples".to_string()
        } else {
            format_us(value)
        }
    };
    let tiles = [
        (
            "Throughput".to_string(),
            format!("{} msg/s", format_number(metrics.throughput_msg_per_sec)),
        ),
        (
            "Error rate".to_string(),
            format!("{:.2}%", metrics.error_rate * 100.0),
        ),
        (format!("{basis} p50"), latency(stats.p50_us)),
        (format!("{basis} p99"), latency(stats.p99_us)),
        (
            "Connections established".to_string(),
            format!(
                "{} / {}",
                metrics.total_connections_established, metrics.total_connections_attempted
            ),
        ),
    ];
    html.push_str("<section class=\"kpis\" aria-label=\"Key numbers\">\n");
    for (label, value) in tiles {
        let _ = writeln!(
            html,
            "<div class=\"kpi\"><p class=\"label\">{}</p><p class=\"value\">{}</p></div>",
            escape(&label),
            escape(&value)
        );
    }
    html.push_str("</section>\n");
}

/// One row per scenario phase, then the SLO checks of every phase that set
/// its own.
fn render_phases(html: &mut String, view: &ReportView<'_>) {
    if view.phases.is_empty() {
        return;
    }
    html.push_str(
        "<section>\n<h2>Phases</h2>\n<div class=\"scroll\"><table>\n<thead><tr>\
         <th scope=\"col\">Phase</th><th scope=\"col\">Target</th><th scope=\"col\">Load</th>\
         <th scope=\"col\" class=\"num\">Throughput (msg/s)</th>\
         <th scope=\"col\" class=\"num\">Error rate</th>\
         <th scope=\"col\" class=\"num\">Gating p99</th><th scope=\"col\">SLO</th>\
         </tr></thead>\n<tbody>\n",
    );
    for phase in view.phases {
        let status = match phase.slo_label() {
            "PASSED" => "<span class=\"ok\">\u{2713} Pass</span>",
            "FAILED" => "<span class=\"ko\">\u{2717} Fail</span>",
            _ => "No SLO",
        };
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{}</th><td><code>{}</code></td><td>{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{:.2}%</td><td class=\"num\">{}</td>\
             <td>{status}</td></tr>",
            escape(&phase.name),
            escape(phase.target_url.as_str()),
            escape(&phase.load_label()),
            escape(&format_number(phase.metrics.throughput_msg_per_sec)),
            phase.metrics.error_rate * 100.0,
            escape(&phase.gating_p99_label()),
        );
    }
    html.push_str("</tbody></table></div>\n");
    for phase in view
        .phases
        .iter()
        .filter(|phase| !phase.slo.checks.is_empty())
    {
        let _ = write!(
            html,
            "<h3 class=\"phase-checks\">Phase {} SLO checks</h3>\n<div class=\"scroll\"><table>\n<thead><tr>\
             <th scope=\"col\">Objective</th><th scope=\"col\">Target</th>\
             <th scope=\"col\">Actual</th><th scope=\"col\">Result</th></tr></thead>\n<tbody>\n",
            escape(&phase.name)
        );
        for check in &phase.slo.checks {
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                escape(&check.name),
                escape(&check.target),
                escape(&check.actual),
                check_result(check.passed)
            );
        }
        html.push_str("</tbody></table></div>\n");
    }
    html.push_str("</section>\n");
}

/// The result cell of an SLO check.
fn check_result(passed: bool) -> &'static str {
    if passed {
        "<span class=\"ok\">\u{2713} Pass</span>"
    } else {
        "<span class=\"ko\">\u{2717} Fail</span>"
    }
}

fn render_latency_table(html: &mut String, view: &ReportView<'_>) {
    let columns = latency_columns(view.metrics);
    html.push_str("<section>\n<h2>Latency percentiles</h2>\n<div class=\"scroll\"><table>\n<thead><tr><th scope=\"col\">Percentile</th>");
    for (name, _) in &columns {
        let _ = write!(
            html,
            "<th scope=\"col\" class=\"num\">{}</th>",
            escape(name)
        );
    }
    html.push_str("</tr></thead>\n<tbody>\n");
    let rows: [PercentileRow; 9] = [
        ("p50", |s| s.p50_us),
        ("p75", |s| s.p75_us),
        ("p90", |s| s.p90_us),
        ("p95", |s| s.p95_us),
        ("p99", |s| s.p99_us),
        ("p99.9", |s| s.p999_us),
        ("p99.99", |s| s.p9999_us),
        ("Min", |s| s.min_us),
        ("Max", |s| s.max_us),
    ];
    for (label, value) in rows {
        let _ = write!(html, "<tr><th scope=\"row\">{label}</th>");
        for (_, stats) in &columns {
            let cell = if stats.count == 0 {
                "n/a".to_string()
            } else {
                format_us(value(stats))
            };
            let _ = write!(html, "<td class=\"num\">{}</td>", escape(&cell));
        }
        html.push_str("</tr>\n");
    }
    html.push_str("<tr><th scope=\"row\">Mean</th>");
    for (_, stats) in &columns {
        let cell = if stats.count == 0 {
            "n/a".to_string()
        } else {
            format_us(stats.mean_us.max(0.0) as u64)
        };
        let _ = write!(html, "<td class=\"num\">{}</td>", escape(&cell));
    }
    html.push_str("</tr>\n<tr><th scope=\"row\">Samples</th>");
    for (_, stats) in &columns {
        let _ = write!(html, "<td class=\"num\">{}</td>", stats.count);
    }
    html.push_str("</tr>\n</tbody></table></div>\n");
    if let Some(hint) = crate::report::coordinated_omission_hint(view.metrics) {
        let _ = writeln!(html, "<p class=\"note\">{}</p>", escape(&hint));
    }
    html.push_str("</section>\n");
}

fn render_errors(html: &mut String, view: &ReportView<'_>) {
    html.push_str("<section>\n<h2>Errors</h2>\n");
    let breakdown = &view.metrics.error_breakdown;
    if breakdown.is_empty() {
        html.push_str("<p class=\"note\">No errors were recorded.</p>\n</section>\n");
        return;
    }
    let mut rows: Vec<(String, String, u64)> = breakdown
        .iter()
        .map(|(category, count)| {
            let code = serde_json::to_value(category)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            (category.to_string(), code, *count)
        })
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    let total: u64 = rows.iter().map(|row| row.2).sum();
    html.push_str(
        "<div class=\"scroll\"><table>\n<thead><tr><th scope=\"col\">Category</th>\
         <th scope=\"col\">Code</th><th scope=\"col\" class=\"num\">Occurrences</th>\
         <th scope=\"col\" class=\"num\">Share</th></tr></thead>\n<tbody>\n",
    );
    for (name, code, count) in rows {
        let share = count as f64 / total.max(1) as f64 * 100.0;
        let _ = writeln!(
            html,
            "<tr><td>{}</td><td><code>{}</code></td><td class=\"num\">{count}</td><td class=\"num\">{share:.1}%</td></tr>",
            escape(&name),
            escape(&code)
        );
    }
    html.push_str("</tbody></table></div>\n</section>\n");
}

fn render_traffic(html: &mut String, view: &ReportView<'_>) {
    let m = view.metrics;
    let mut rows: Vec<(&str, String)> = vec![
        (
            "Connections attempted",
            m.total_connections_attempted.to_string(),
        ),
        (
            "Connections established",
            m.total_connections_established.to_string(),
        ),
        ("Connections failed", m.total_connections_failed.to_string()),
    ];
    if m.total_connections_interrupted > 0 {
        rows.push((
            "Connections interrupted (still connecting at end of run)",
            m.total_connections_interrupted.to_string(),
        ));
    }
    if m.total_connections_not_started > 0 {
        rows.push((
            "Connections not started (ramp cut short by --duration)",
            m.total_connections_not_started.to_string(),
        ));
    }
    rows.push(("Reconnects", m.total_reconnects.to_string()));
    rows.push((
        "Messages sent / received",
        format!("{} / {}", m.total_messages_sent, m.total_messages_recv),
    ));
    if m.total_unsolicited_frames > 0 {
        rows.push((
            "Unsolicited frames (skipped by --match)",
            m.total_unsolicited_frames.to_string(),
        ));
    }
    rows.push((
        "Data sent / received",
        format!(
            "{:.2} MB / {:.2} MB",
            m.total_bytes_sent as f64 / (1024.0 * 1024.0),
            m.total_bytes_recv as f64 / (1024.0 * 1024.0)
        ),
    ));
    rows.push((
        "Bandwidth",
        format!("{:.2} MB/s", m.throughput_bytes_per_sec / (1024.0 * 1024.0)),
    ));
    if let Some(capture) = &m.capture {
        rows.push((
            "Capture",
            format!(
                "{} frames written, {} dropped ({})",
                capture.frames_written, capture.frames_dropped, capture.path
            ),
        ));
    }
    if let Some(chaos) = &m.chaos {
        rows.push(("Chaos", crate::report::chaos_label(chaos)));
    }
    if let Some(generator) = crate::report::generator_label(m) {
        rows.push(("Generator", generator));
    }
    html.push_str(
        "<section>\n<h2>Connections and traffic</h2>\n<div class=\"scroll\"><table>\n<tbody>\n",
    );
    for (label, value) in rows {
        let _ = writeln!(
            html,
            "<tr><th scope=\"row\">{}</th><td class=\"num\">{}</td></tr>",
            escape(label),
            escape(&value)
        );
    }
    html.push_str("</tbody></table></div>\n</section>\n");
}

fn render_charts(html: &mut String, view: &ReportView<'_>) {
    let series = &view.metrics.time_series;
    html.push_str("<section>\n<h2>Over time</h2>\n");
    if series.points.is_empty() {
        html.push_str(
            "<p class=\"note\">This report has no time series. Reports written by wsblast 0.2 \
             and earlier do not record one.</p>\n</section>\n",
        );
        return;
    }
    let basis = match series.latency_basis.as_str() {
        "response_time" => "response time",
        "mixed" => "latency (RTT or response time by phase)",
        _ => "RTT",
    };
    let _ = writeln!(
        html,
        "<p class=\"note\">{} intervals{}. Each point is one interval of the time series; \
         hover a chart for the value of an interval, or open its data table.</p>",
        series.points.len(),
        if series.interval_secs > 0.0 {
            format!(" of {}", format_secs(series.interval_secs))
        } else {
            String::new()
        }
    );
    html.push_str("<div class=\"charts\">\n");
    let points = &series.points;
    let charts = [
        Chart {
            id: "throughput",
            title: "Throughput per interval".to_string(),
            unit: "msg/s",
            kind: ChartKind::Line,
            integer: false,
            allow_log: false,
            values: points
                .iter()
                .map(|p| finite(p.throughput_msg_per_sec))
                .collect(),
            caption: "The larger of messages sent and received, per second of the interval."
                .to_string(),
        },
        Chart {
            id: "latency",
            title: format!("p99 {basis} per interval"),
            unit: "ms",
            kind: ChartKind::Line,
            integer: false,
            allow_log: true,
            values: points
                .iter()
                .map(|p| (p.latency.count > 0).then(|| p.latency.p99_us as f64 / 1_000.0))
                .collect(),
            caption: "Latency of the replies that arrived in each interval; intervals \
                      without replies are left as gaps."
                .to_string(),
        },
        Chart {
            id: "connections",
            title: "Active connections".to_string(),
            unit: "connections",
            kind: ChartKind::Line,
            integer: true,
            allow_log: false,
            values: points
                .iter()
                .map(|p| Some(p.active_connections as f64))
                .collect(),
            caption: "Connections open at the end of each interval.".to_string(),
        },
        Chart {
            id: "errors",
            title: "Errors per interval".to_string(),
            unit: "errors",
            kind: ChartKind::Bars,
            integer: true,
            allow_log: false,
            values: points.iter().map(|p| Some(p.errors as f64)).collect(),
            caption: "Errors of every category recorded in each interval.".to_string(),
        },
    ];
    for chart in &charts {
        render_chart(html, chart, points);
    }
    html.push_str("</div>\n</section>\n");
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value.max(0.0))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChartKind {
    Line,
    Bars,
}

/// One chart: a value (or a gap) per time series point.
struct Chart {
    id: &'static str,
    title: String,
    unit: &'static str,
    kind: ChartKind,
    /// Counts: ticks are whole numbers.
    integer: bool,
    /// Switches to a log scale when the values span more than
    /// [`LOG_SPAN`] times, so a stall does not flatten the rest.
    allow_log: bool,
    values: Vec<Option<f64>>,
    caption: String,
}

const WIDTH: f64 = 640.0;
const HEIGHT: f64 = 240.0;
const LEFT: f64 = 56.0;
const RIGHT: f64 = 14.0;
const TOP: f64 = 22.0;
const BOTTOM: f64 = 30.0;
/// The ratio of largest to smallest value above which a chart that allows
/// it uses a log scale.
const LOG_SPAN: f64 = 50.0;

/// A y scale from data values to SVG coordinates.
enum Scale {
    Linear { max: f64, step: f64 },
    Log { lo: f64, hi: f64 },
}

impl Scale {
    fn y(&self, value: f64) -> f64 {
        let plot = HEIGHT - TOP - BOTTOM;
        let fraction = match *self {
            Self::Linear { max, .. } => value / max,
            Self::Log { lo, hi } => {
                (value.max(lo).log10() - lo.log10()) / (hi.log10() - lo.log10())
            }
        };
        TOP + plot * (1.0 - fraction.clamp(0.0, 1.0))
    }

    fn ticks(&self) -> Vec<f64> {
        match *self {
            Self::Linear { max, step } => (0..)
                .map(|i| f64::from(i) * step)
                .take_while(|tick| *tick <= max * (1.0 + 1e-9))
                .take(20)
                .collect(),
            Self::Log { lo, hi } => {
                let mut ticks = Vec::new();
                let mut tick = lo;
                while tick <= hi * (1.0 + 1e-9) {
                    ticks.push(tick);
                    tick *= 10.0;
                }
                ticks
            }
        }
    }
}

fn scale_for(chart: &Chart) -> Scale {
    let present = chart.values.iter().flatten().copied();
    let max = present.clone().fold(0.0_f64, f64::max);
    let min_positive = present.filter(|v| *v > 0.0).fold(f64::INFINITY, f64::min);
    if chart.allow_log && min_positive.is_finite() && max / min_positive > LOG_SPAN {
        let lo = 10f64.powf(min_positive.log10().floor());
        let mut hi = 10f64.powf(max.log10().ceil());
        if hi <= lo {
            hi = lo * 10.0;
        }
        return Scale::Log { lo, hi };
    }
    // An all-zero series gets a 0 to 1 axis.
    let span = if max > 0.0 { max } else { 1.0 };
    let mut step = nice_step(span, 4.0);
    if chart.integer {
        step = step.max(1.0);
    }
    Scale::Linear {
        max: ((span / step).ceil() * step).max(step),
        step,
    }
}

/// A 1, 2 or 5 times a power of ten step that divides `span` into about
/// `count` parts.
fn nice_step(span: f64, count: f64) -> f64 {
    if !(span.is_finite() && span > 0.0) {
        return 1.0;
    }
    let raw = span / count;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalized = raw / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// A short axis label: `0`, `0.25`, `12`, `1.5k`, `2M`.
fn format_tick(value: f64) -> String {
    let trim = |text: String| {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text
        }
    };
    let abs = value.abs();
    if abs == 0.0 {
        "0".to_string()
    } else if abs >= 1e6 {
        format!("{}M", trim(format!("{:.2}", value / 1e6)))
    } else if abs >= 1e4 {
        format!("{}k", trim(format!("{:.1}", value / 1e3)))
    } else if abs >= 10.0 {
        format!("{value:.0}")
    } else if abs >= 1.0 {
        trim(format!("{value:.2}"))
    } else {
        trim(format!("{value:.3}"))
    }
}

/// A value for tooltips, tables and descriptions.
fn format_number(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn format_value(chart: &Chart, value: f64) -> String {
    if chart.integer {
        format!("{value:.0} {}", chart.unit)
    } else {
        format!("{} {}", format_number(value), chart.unit)
    }
}

/// The time axis: seconds, or minutes for runs longer than ten minutes.
struct TimeAxis {
    x0: f64,
    span: f64,
}

impl TimeAxis {
    fn new(points: &[TimeSeriesPoint]) -> Self {
        let x0 = points
            .iter()
            .map(|p| p.start_secs)
            .filter(|v| v.is_finite())
            .fold(f64::INFINITY, f64::min);
        let x0 = if x0.is_finite() { x0 } else { 0.0 };
        let x1 = points
            .iter()
            .map(|p| p.start_secs + p.duration_secs.max(0.0))
            .filter(|v| v.is_finite())
            .fold(x0, f64::max);
        let span = x1 - x0;
        Self {
            x0,
            span: if span > 0.0 { span } else { 1.0 },
        }
    }

    fn x(&self, secs: f64) -> f64 {
        let fraction = if secs.is_finite() {
            ((secs - self.x0) / self.span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        LEFT + (WIDTH - LEFT - RIGHT) * fraction
    }

    fn ticks(&self) -> Vec<(f64, String)> {
        let minutes = self.span > 600.0;
        let unit = if minutes { 60.0 } else { 1.0 };
        let step = nice_step(self.span / unit, 5.0) * unit;
        let first = (self.x0 / step).ceil() * step;
        (0..)
            .map(|i| first + f64::from(i) * step)
            .take_while(|tick| *tick <= self.x0 + self.span * (1.0 + 1e-9))
            .take(50)
            .map(|tick| {
                let label = if minutes {
                    format!("{}m", format_tick(tick / 60.0))
                } else {
                    format!("{}s", format_tick(tick))
                };
                (tick, label)
            })
            .collect()
    }
}

/// Whether `next` starts noticeably later than `previous` ends, as across
/// the pause between scenario phases.
fn is_time_gap(previous: &TimeSeriesPoint, next: &TimeSeriesPoint) -> bool {
    let end = previous.start_secs + previous.duration_secs.max(0.0);
    next.start_secs - end > 0.25 * previous.duration_secs.max(0.001)
}

/// The unit suffix of a chart heading, left out when the title already
/// names the unit.
fn heading_unit(chart: &Chart, log: bool) -> String {
    let named = chart.title.to_lowercase().contains(chart.unit);
    match (named, log) {
        (true, false) => String::new(),
        (true, true) => " (log scale)".to_string(),
        (false, false) => format!(" ({})", chart.unit),
        (false, true) => format!(" ({}, log scale)", chart.unit),
    }
}

fn render_chart(html: &mut String, chart: &Chart, points: &[TimeSeriesPoint]) {
    let axis = TimeAxis::new(points);
    let scale = scale_for(chart);
    let log = matches!(scale, Scale::Log { .. });
    let title_id = format!("chart-{}-title", chart.id);
    let desc_id = format!("chart-{}-desc", chart.id);
    let description = describe(chart, points);

    let _ = write!(
        html,
        "<figure>\n<h3 id=\"chart-{id}-heading\">{title}{unit}</h3>\n\
         <svg class=\"chart\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" role=\"img\" \
         aria-labelledby=\"{title_id} {desc_id}\">\n\
         <title id=\"{title_id}\">{title}</title>\n<desc id=\"{desc_id}\">{desc}</desc>\n",
        id = chart.id,
        title = escape(&chart.title),
        unit = escape(&heading_unit(chart, log)),
        desc = escape(&description),
    );

    // Grid lines and y labels.
    for tick in scale.ticks() {
        let y = scale.y(tick);
        let _ = writeln!(
            html,
            "<line class=\"grid\" x1=\"{LEFT}\" x2=\"{:.1}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>\
             <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            WIDTH - RIGHT,
            LEFT - 6.0,
            y + 4.0,
            escape(&format_tick(tick))
        );
    }
    let _ = writeln!(
        html,
        "<text x=\"{LEFT}\" y=\"12\">{}</text>",
        escape(chart.unit)
    );
    // The time axis.
    let baseline = HEIGHT - BOTTOM;
    let _ = writeln!(
        html,
        "<line class=\"axis\" x1=\"{LEFT}\" x2=\"{:.1}\" y1=\"{baseline:.1}\" y2=\"{baseline:.1}\"/>",
        WIDTH - RIGHT
    );
    for (tick, label) in axis.ticks() {
        let x = axis.x(tick);
        let _ = writeln!(
            html,
            "<line class=\"axis\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{baseline:.1}\" y2=\"{:.1}\"/>\
             <text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            baseline + 4.0,
            baseline + 17.0,
            escape(&label)
        );
    }

    let present = chart.values.iter().flatten().count();
    if present == 0 {
        let _ = writeln!(
            html,
            "<text class=\"empty\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">No data in any interval</text>",
            LEFT + (WIDTH - LEFT - RIGHT) / 2.0,
            TOP + (HEIGHT - TOP - BOTTOM) / 2.0
        );
    }

    match chart.kind {
        ChartKind::Line => render_line(html, chart, points, &axis, &scale),
        ChartKind::Bars => render_bars(html, chart, points, &axis, &scale),
    }

    // Hover targets: one transparent column per interval with its value.
    for (point, value) in points.iter().zip(&chart.values) {
        let x0 = axis.x(point.start_secs);
        let x1 = axis.x(point.start_secs + point.duration_secs.max(0.0));
        let label = match value {
            Some(value) => format_value(chart, *value),
            None => "no replies".to_string(),
        };
        let _ = writeln!(
            html,
            "<rect class=\"hit\" x=\"{x0:.1}\" y=\"{TOP}\" width=\"{:.1}\" height=\"{:.1}\">\
             <title>{} to {}: {}</title></rect>",
            (x1 - x0).max(1.0),
            HEIGHT - TOP - BOTTOM,
            escape(&format_secs(point.start_secs)),
            escape(&format_secs(point.start_secs + point.duration_secs)),
            escape(&label)
        );
    }
    html.push_str("</svg>\n");
    let _ = writeln!(html, "<figcaption>{}</figcaption>", escape(&chart.caption));
    render_chart_table(html, chart, points);
    html.push_str("</figure>\n");
}

/// Draws a line through the middle of each interval, broken at intervals
/// without a value and at gaps in time. A point with no neighbour to join
/// is drawn as a dot.
fn render_line(
    html: &mut String,
    chart: &Chart,
    points: &[TimeSeriesPoint],
    axis: &TimeAxis,
    scale: &Scale,
) {
    let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut previous: Option<&TimeSeriesPoint> = None;
    for (point, value) in points.iter().zip(&chart.values) {
        let gap = previous.is_some_and(|prev| is_time_gap(prev, point));
        if (value.is_none() || gap) && !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
        if let Some(value) = value {
            let x = axis.x(point.start_secs + point.duration_secs.max(0.0) / 2.0);
            current.push((x, scale.y(*value)));
        }
        previous = Some(point);
    }
    if !current.is_empty() {
        runs.push(current);
    }
    let mut path = String::new();
    let mut dots = String::new();
    for run in &runs {
        if let [(x, y)] = run.as_slice() {
            let _ = write!(
                dots,
                "<circle class=\"dot\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"4\"/>"
            );
            continue;
        }
        for (i, (x, y)) in run.iter().enumerate() {
            let _ = write!(path, "{}{x:.1} {y:.1}", if i == 0 { "M" } else { "L" });
        }
    }
    if !path.is_empty() {
        let _ = writeln!(html, "<path class=\"line\" d=\"{path}\"/>");
    }
    if !dots.is_empty() {
        let _ = writeln!(html, "{dots}");
    }
}

fn render_bars(
    html: &mut String,
    chart: &Chart,
    points: &[TimeSeriesPoint],
    axis: &TimeAxis,
    scale: &Scale,
) {
    let baseline = scale.y(0.0);
    for (point, value) in points.iter().zip(&chart.values) {
        let Some(value) = value.filter(|v| *v > 0.0) else {
            continue;
        };
        let x0 = axis.x(point.start_secs);
        let x1 = axis.x(point.start_secs + point.duration_secs.max(0.0));
        // A 1-unit gap between neighbouring bars when they are wide enough.
        let inset = if x1 - x0 > 3.0 { 0.5 } else { 0.0 };
        let y = scale.y(value).min(baseline - 1.0);
        let _ = writeln!(
            html,
            "<rect class=\"bar\" x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"1\"/>",
            x0 + inset,
            (x1 - x0 - 2.0 * inset).max(0.8),
            baseline - y
        );
    }
}

fn render_chart_table(html: &mut String, chart: &Chart, points: &[TimeSeriesPoint]) {
    let _ = write!(
        html,
        "<details>\n<summary>Data table ({} interval{})</summary>\n<div class=\"scroll\"><table>\n\
         <thead><tr><th scope=\"col\">Start</th><th scope=\"col\">End</th>\
         <th scope=\"col\" class=\"num\">{} ({})</th></tr></thead>\n<tbody>\n",
        points.len(),
        if points.len() == 1 { "" } else { "s" },
        escape(&chart.title),
        escape(chart.unit)
    );
    for (point, value) in points.iter().zip(&chart.values) {
        let cell = match value {
            Some(value) if chart.integer => format!("{value:.0}"),
            Some(value) => format_number(*value),
            None => "no replies".to_string(),
        };
        let _ = writeln!(
            html,
            "<tr><td>{}</td><td>{}</td><td class=\"num\">{}</td></tr>",
            escape(&format_secs(point.start_secs)),
            escape(&format_secs(point.start_secs + point.duration_secs)),
            escape(&cell)
        );
    }
    html.push_str("</tbody></table></div>\n</details>\n");
}

/// The chart's text alternative: the extent of the series and its extremes.
fn describe(chart: &Chart, points: &[TimeSeriesPoint]) -> String {
    let axis = TimeAxis::new(points);
    let mut text = format!(
        "{} interval{} from {} to {}.",
        points.len(),
        if points.len() == 1 { "" } else { "s" },
        format_secs(axis.x0),
        format_secs(axis.x0 + axis.span)
    );
    let present: Vec<(&TimeSeriesPoint, f64)> = points
        .iter()
        .zip(&chart.values)
        .filter_map(|(point, value)| value.map(|v| (point, v)))
        .collect();
    let Some(&(max_point, max)) = present.iter().max_by(|a, b| a.1.total_cmp(&b.1)) else {
        text.push_str(" No interval has a value.");
        return text;
    };
    if max == 0.0 {
        text.push_str(" Every value is zero.");
        return text;
    }
    let &(min_point, min) = present
        .iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("present is not empty");
    let _ = write!(
        text,
        " Highest {} in the interval starting at {}; lowest {} at {}.",
        format_value(chart, max),
        format_secs(max_point.start_secs),
        format_value(chart, min),
        format_secs(min_point.start_secs)
    );
    let missing = points.len() - present.len();
    if missing > 0 {
        let _ = write!(
            text,
            " {missing} interval{} had no replies.",
            if missing == 1 { "" } else { "s" }
        );
    }
    text
}

#[cfg(feature = "compare")]
pub(crate) use comparison::render_html_comparison;

/// The `--compare --format html` page, in the style of the run report.
#[cfg(feature = "compare")]
mod comparison {
    use super::{BOTTOM, HEIGHT, LEFT, RIGHT, TOP, WIDTH};
    use super::{
        Chart, ChartKind, STYLE, Scale, TimeAxis, describe, escape, finite, format_number,
        readable_timestamp, render_line, scale_for,
    };
    use crate::report::{
        ChangeOutcome, ComparedMetric, ComparisonSource, ComparisonView, format_secs, format_us,
        higher_is_better,
    };
    use crate::series_compare::CompareWindow;
    use crate::timeseries::{TimeSeries, TimeSeriesPoint};
    use std::fmt::Write;

    const COMPARE_STYLE: &str = r#"
tr.regressed > * { background: var(--bad-bg); }
tr.improved > * { background: var(--good-bg); }
.legend { display: flex; flex-wrap: wrap; gap: 4px 16px; margin: 0 0 6px; color: var(--text-2); font-size: 0.85rem; }
.swatch { display: inline-block; width: 22px; vertical-align: middle; margin-right: 6px; border-top: 2px dashed var(--series); }
.swatch.current { border-top: 2px solid var(--series-2); }
svg.chart .baseline .line { stroke-dasharray: 6 4; }
svg.chart .current .line { stroke: var(--series-2); }
svg.chart .current .dot { fill: var(--series-2); }
svg.chart .window { fill: var(--hover); }
"#;

    /// Renders the comparison page.
    pub(crate) fn render_html_comparison(view: &ComparisonView<'_>) -> String {
        let mut html = String::with_capacity(32 * 1024);
        let _ = write!(
            html,
            "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <meta name=\"color-scheme\" content=\"light dark\">\n\
             <title>wsblast baseline comparison</title>\n<style>{STYLE}{COMPARE_STYLE}</style>\n\
             </head>\n<body>\n<main>\n"
        );
        render_header(&mut html, view);
        render_verdict(&mut html, view);
        render_metric_table(&mut html, view);
        render_overlay_charts(&mut html, view);
        let _ = write!(
            html,
            "<footer>Generated by wsblast {}. A metric regressed or improved when it moved \
             by more than the threshold in its worse or better direction; see docs/ci.md \
             section 7 for the rules and METHODOLOGY.md section 6.5 for the time-series \
             metrics.</footer>\n</main>\n</body>\n</html>\n",
            env!("CARGO_PKG_VERSION")
        );
        html
    }

    fn render_header(html: &mut String, view: &ComparisonView<'_>) {
        html.push_str("<header>\n<h1>wsblast baseline comparison</h1>\n<dl class=\"meta\">\n");
        match &view.source {
            ComparisonSource::Reports {
                files: Some((baseline, current)),
                ..
            } => {
                let _ = writeln!(
                    html,
                    "<dt>Baseline</dt><dd><code>{}</code></dd>\n\
                     <dt>Current</dt><dd><code>{}</code></dd>",
                    escape(baseline),
                    escape(current)
                );
            }
            ComparisonSource::Reports { files: None, .. } => {}
            ComparisonSource::Saved {
                schema_version,
                timestamp,
            } => {
                let _ = writeln!(
                    html,
                    "<dt>Source</dt><dd>Comparison JSON, schema {}</dd>",
                    escape(schema_version)
                );
                if let Some((label, time)) = timestamp {
                    let _ = writeln!(
                        html,
                        "<dt>{}</dt><dd>{}</dd>",
                        escape(label),
                        escape(&readable_timestamp(time))
                    );
                }
            }
        }
        let _ = writeln!(
            html,
            "<dt>Threshold</dt><dd>{}</dd>",
            escape(&format!("\u{b1}{:.1}%", view.threshold))
        );
        if let Some(window) = &view.window {
            let _ = writeln!(
                html,
                "<dt>Window</dt><dd><code>{}</code> (time-series metrics only)</dd>",
                escape(&window.to_string())
            );
        }
        html.push_str("</dl>\n</header>\n");
    }

    fn plural(count: usize, one: &str, many: &str) -> String {
        format!("{count} {}", if count == 1 { one } else { many })
    }

    fn render_verdict(html: &mut String, view: &ComparisonView<'_>) {
        let regressed = view.regressions.len();
        let improved = view.improvements.len();
        let threshold = format!("\u{b1}{:.1}%", view.threshold);
        let (class, icon, verdict) = if regressed > 0 {
            let mut verdict = format!(
                "REGRESSION: {} beyond {threshold}",
                plural(regressed, "metric regressed", "metrics regressed")
            );
            if improved > 0 {
                let _ = write!(
                    verdict,
                    ", {} improved",
                    plural(improved, "metric", "metrics")
                );
            }
            ("fail", "\u{2717} ", verdict)
        } else if improved > 0 {
            (
                "pass",
                "\u{2713} ",
                format!(
                    "No regressions: {} beyond {threshold}",
                    plural(improved, "metric improved", "metrics improved")
                ),
            )
        } else {
            (
                "pass",
                "\u{2713} ",
                format!("No regressions: no metric changed by more than {threshold}"),
            )
        };
        let _ = writeln!(
            html,
            "<section class=\"banner {class}\" aria-label=\"Comparison verdict\">\n\
             <p class=\"verdict\">{icon}{}</p>\n<p>With <code>--fail-on-regression</code> this \
             comparison exits with code {}.</p>\n</section>",
            escape(&verdict),
            if regressed > 0 { 2 } else { 0 }
        );

        let compared = match &view.source {
            ComparisonSource::Reports { compared, .. } => Some(compared.len()),
            ComparisonSource::Saved { .. } => None,
        };
        html.push_str("<div class=\"kpis\">\n");
        for (label, value) in [
            ("Regressions", Some(regressed)),
            ("Improvements", Some(improved)),
            ("Metrics compared", compared),
        ] {
            if let Some(value) = value {
                let _ = writeln!(
                    html,
                    "<div class=\"kpi\"><p class=\"label\">{label}</p><p class=\"value\">{value}</p></div>"
                );
            }
        }
        html.push_str("</div>\n");
    }

    /// A metric value in its unit: latencies in µs, ms or s, the error rate
    /// in percent, counts as integers.
    fn format_metric(metric: &str, value: f64) -> String {
        if !value.is_finite() {
            return "n/a".to_string();
        }
        if metric.ends_with("_us") {
            return format_us(value.round().max(0.0) as u64);
        }
        if metric == "error_rate" {
            return format!("{:.3}%", value * 100.0);
        }
        if metric.ends_with("_cv") {
            return format!("{value:.3}");
        }
        if value.fract() == 0.0 && value.abs() < 1e15 {
            return format!("{value:.0}");
        }
        format!("{value:.2}")
    }

    fn format_change(row: &ComparedMetric) -> String {
        if !row.pct_change.is_finite() {
            return "n/a".to_string();
        }
        if row.baseline == 0.0 && row.current != 0.0 {
            return format!("{:+.2}% (from 0)", row.pct_change);
        }
        format!("{:+.2}%", row.pct_change)
    }

    /// The rows of the table: every compared metric when the comparison was
    /// computed here, the changes beyond the threshold for a saved one.
    /// Regressions come first, then improvements, then the rest, each in
    /// metric-name order.
    fn table_rows(view: &ComparisonView<'_>) -> Vec<ComparedMetric> {
        let mut rows: Vec<ComparedMetric> = match &view.source {
            ComparisonSource::Reports { compared, .. } => compared.to_vec(),
            ComparisonSource::Saved { .. } => {
                let changes = view
                    .regressions
                    .iter()
                    .map(|r| {
                        (
                            &r.metric,
                            r.baseline,
                            r.current,
                            r.pct_change,
                            ChangeOutcome::Regression,
                        )
                    })
                    .chain(view.improvements.iter().map(|i| {
                        (
                            &i.metric,
                            i.baseline,
                            i.current,
                            i.pct_change,
                            ChangeOutcome::Improvement,
                        )
                    }));
                changes
                    .map(
                        |(metric, baseline, current, pct_change, outcome)| ComparedMetric {
                            metric: metric.clone(),
                            baseline,
                            current,
                            pct_change,
                            higher_is_better: higher_is_better(metric),
                            outcome,
                        },
                    )
                    .collect()
            }
        };
        let rank = |outcome: ChangeOutcome| match outcome {
            ChangeOutcome::Regression => 0,
            ChangeOutcome::Improvement => 1,
            ChangeOutcome::WithinThreshold => 2,
        };
        rows.sort_by(|a, b| {
            rank(a.outcome)
                .cmp(&rank(b.outcome))
                .then_with(|| a.metric.cmp(&b.metric))
        });
        rows
    }

    fn render_metric_table(html: &mut String, view: &ComparisonView<'_>) {
        let rows = table_rows(view);
        let saved = matches!(view.source, ComparisonSource::Saved { .. });
        html.push_str("<section>\n<h2>Compared metrics</h2>\n");
        if saved {
            html.push_str(
                "<p class=\"note\">A comparison JSON records only the metrics that moved by more \
                 than the threshold, so metrics within it are not listed. Run <code>wsblast \
                 --compare BASELINE CURRENT --format html</code> on the two reports for the full \
                 table and the charts.</p>\n",
            );
        } else {
            html.push_str(
                "<p class=\"note\">Every metric both reports have, except those that are zero in \
                 both. Regressions are highlighted first, then improvements.</p>\n",
            );
        }
        if rows.is_empty() {
            html.push_str("<p>No metric to list.</p>\n</section>\n");
            return;
        }
        html.push_str(
            "<div class=\"scroll\"><table>\n<thead><tr><th scope=\"col\">Metric</th>\
             <th scope=\"col\" class=\"num\">Baseline</th><th scope=\"col\" class=\"num\">Current</th>\
             <th scope=\"col\" class=\"num\">Change</th><th scope=\"col\">Better when</th>\
             <th scope=\"col\">Result</th></tr></thead>\n<tbody>\n",
        );
        for row in &rows {
            let (class, result) = match row.outcome {
                ChangeOutcome::Regression => (
                    " class=\"regressed\"",
                    "<span class=\"ko\">\u{2717} regression</span>",
                ),
                ChangeOutcome::Improvement => (
                    " class=\"improved\"",
                    "<span class=\"ok\">\u{2713} improvement</span>",
                ),
                ChangeOutcome::WithinThreshold => ("", "within threshold"),
            };
            let _ = writeln!(
                html,
                "<tr{class}><th scope=\"row\"><code>{}</code></th><td class=\"num\">{}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{result}</td></tr>",
                escape(&row.metric),
                escape(&format_metric(&row.metric, row.baseline)),
                escape(&format_metric(&row.metric, row.current)),
                escape(&format_change(row)),
                if row.higher_is_better {
                    "higher"
                } else {
                    "lower"
                },
            );
        }
        html.push_str("</tbody></table></div>\n</section>\n");
    }

    fn render_overlay_charts(html: &mut String, view: &ComparisonView<'_>) {
        html.push_str("<section>\n<h2>Over time: baseline and current</h2>\n");
        let (baseline, current) = match &view.source {
            ComparisonSource::Saved { .. } => {
                html.push_str(
                    "<p class=\"note\">A comparison JSON does not include the time series of the \
                     two runs, so there are no charts.</p>\n</section>\n",
                );
                return;
            }
            ComparisonSource::Reports {
                baseline, current, ..
            } => (&baseline.time_series, &current.time_series),
        };
        let missing: Vec<&str> = [("baseline", baseline), ("current", current)]
            .into_iter()
            .filter(|(_, series)| series.points.is_empty())
            .map(|(name, _)| name)
            .collect();
        if !missing.is_empty() {
            let _ = writeln!(
                html,
                "<p class=\"note\">The charts need a time series in both reports, and the {} \
                 report{} none. Reports written by wsblast 0.2 and earlier do not record \
                 one.</p>\n</section>",
                missing.join(" and the "),
                if missing.len() == 1 { " has" } else { "s have" }
            );
            return;
        }
        let _ = writeln!(
            html,
            "<p class=\"note\">Baseline: {}; current: {}. Both are plotted against the time since \
             their run started{}.</p>",
            escape(&interval_summary(baseline)),
            escape(&interval_summary(current)),
            if view.window.is_some() {
                "; the shaded band is the comparison window"
            } else {
                ""
            }
        );
        html.push_str(
            "<p class=\"legend\"><span><span class=\"swatch\"></span>Baseline</span>\
             <span><span class=\"swatch current\"></span>Current</span></p>\n",
        );
        html.push_str("<div class=\"charts\">\n");
        let basis = latency_basis(baseline, current);
        let throughput = |series: &TimeSeries| -> Vec<Option<f64>> {
            series
                .points
                .iter()
                .map(|p| finite(p.throughput_msg_per_sec))
                .collect()
        };
        let p99 = |series: &TimeSeries| -> Vec<Option<f64>> {
            series
                .points
                .iter()
                .map(|p| (p.latency.count > 0).then(|| p.latency.p99_us as f64 / 1_000.0))
                .collect()
        };
        render_overlay(
            html,
            &Overlay {
                id: "throughput",
                title: "Throughput per interval".to_string(),
                unit: "msg/s",
                allow_log: false,
                caption: "The larger of messages sent and received, per second of each interval."
                    .to_string(),
            },
            (baseline, throughput(baseline)),
            (current, throughput(current)),
            view.window.as_ref(),
        );
        render_overlay(
            html,
            &Overlay {
                id: "latency",
                title: format!("p99 {basis} per interval"),
                unit: "ms",
                allow_log: true,
                caption: "Latency of the replies that arrived in each interval; intervals \
                          without replies are left as gaps."
                    .to_string(),
            },
            (baseline, p99(baseline)),
            (current, p99(current)),
            view.window.as_ref(),
        );
        html.push_str("</div>\n</section>\n");
    }

    fn interval_summary(series: &TimeSeries) -> String {
        let count = series.points.len();
        let mut text = format!("{count} interval{}", if count == 1 { "" } else { "s" });
        if series.interval_secs > 0.0 {
            let _ = write!(text, " of {}", format_secs(series.interval_secs));
        }
        text
    }

    fn latency_basis(baseline: &TimeSeries, current: &TimeSeries) -> &'static str {
        let name = |series: &TimeSeries| match series.latency_basis.as_str() {
            "response_time" => "response time",
            "mixed" => "latency",
            _ => "RTT",
        };
        let (a, b) = (name(baseline), name(current));
        if a == b { a } else { "latency" }
    }

    struct Overlay {
        id: &'static str,
        title: String,
        unit: &'static str,
        allow_log: bool,
        caption: String,
    }

    fn chart(overlay: &Overlay, title: String, values: Vec<Option<f64>>) -> Chart {
        Chart {
            id: overlay.id,
            title,
            unit: overlay.unit,
            kind: ChartKind::Line,
            integer: false,
            allow_log: overlay.allow_log,
            values,
            caption: String::new(),
        }
    }

    /// One chart with a line per run on shared axes: time since each run
    /// started, and one value scale over both runs.
    fn render_overlay(
        html: &mut String,
        overlay: &Overlay,
        baseline: (&TimeSeries, Vec<Option<f64>>),
        current: (&TimeSeries, Vec<Option<f64>>),
        window: Option<&CompareWindow>,
    ) {
        let (baseline_series, baseline_values) = baseline;
        let (current_series, current_values) = current;
        let both_points: Vec<TimeSeriesPoint> = baseline_series
            .points
            .iter()
            .chain(&current_series.points)
            .cloned()
            .collect();
        let both = chart(
            overlay,
            overlay.title.clone(),
            baseline_values
                .iter()
                .chain(&current_values)
                .copied()
                .collect(),
        );
        let axis = TimeAxis::new(&both_points);
        let scale = scale_for(&both);
        let log = matches!(scale, Scale::Log { .. });
        let baseline_chart = chart(
            overlay,
            format!("Baseline {}", overlay.title),
            baseline_values,
        );
        let current_chart = chart(
            overlay,
            format!("Current {}", overlay.title),
            current_values,
        );
        let description = format!(
            "Baseline: {} Current: {}",
            describe(&baseline_chart, &baseline_series.points),
            describe(&current_chart, &current_series.points)
        );
        let unit = match (log, overlay.title.to_lowercase().contains(overlay.unit)) {
            (false, true) => String::new(),
            (true, true) => " (log scale)".to_string(),
            (false, false) => format!(" ({})", overlay.unit),
            (true, false) => format!(" ({}, log scale)", overlay.unit),
        };
        let title_id = format!("overlay-{}-title", overlay.id);
        let desc_id = format!("overlay-{}-desc", overlay.id);
        let _ = write!(
            html,
            "<figure>\n<h3>{title}{unit}</h3>\n\
             <svg class=\"chart\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" role=\"img\" \
             aria-labelledby=\"{title_id} {desc_id}\">\n\
             <title id=\"{title_id}\">{title}: baseline and current</title>\n\
             <desc id=\"{desc_id}\">{desc}</desc>\n",
            title = escape(&overlay.title),
            unit = escape(&unit),
            desc = escape(&description),
        );

        if let Some(window) = window {
            let start = window.start.map_or(axis.x0, |start| start.as_secs_f64());
            let end = window
                .end
                .map_or(axis.x0 + axis.span, |end| end.as_secs_f64());
            let (x0, x1) = (axis.x(start), axis.x(end));
            if x1 > x0 {
                let _ = writeln!(
                    html,
                    "<rect class=\"window\" x=\"{x0:.1}\" y=\"{TOP}\" width=\"{:.1}\" height=\"{:.1}\">\
                     <title>Comparison window {}</title></rect>",
                    x1 - x0,
                    HEIGHT - TOP - BOTTOM,
                    escape(&window.to_string())
                );
            }
        }
        for tick in scale.ticks() {
            let y = scale.y(tick);
            let _ = writeln!(
                html,
                "<line class=\"grid\" x1=\"{LEFT}\" x2=\"{:.1}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
                WIDTH - RIGHT,
                LEFT - 6.0,
                y + 4.0,
                escape(&super::format_tick(tick))
            );
        }
        let _ = writeln!(
            html,
            "<text x=\"{LEFT}\" y=\"12\">{}</text>",
            escape(overlay.unit)
        );
        let axis_y = HEIGHT - BOTTOM;
        let _ = writeln!(
            html,
            "<line class=\"axis\" x1=\"{LEFT}\" x2=\"{:.1}\" y1=\"{axis_y:.1}\" y2=\"{axis_y:.1}\"/>",
            WIDTH - RIGHT
        );
        for (tick, label) in axis.ticks() {
            let x = axis.x(tick);
            let _ = writeln!(
                html,
                "<line class=\"axis\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{axis_y:.1}\" y2=\"{:.1}\"/>\
                 <text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
                axis_y + 4.0,
                axis_y + 17.0,
                escape(&label)
            );
        }
        if both.values.iter().flatten().next().is_none() {
            let _ = writeln!(
                html,
                "<text class=\"empty\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">No data in any interval</text>",
                LEFT + (WIDTH - LEFT - RIGHT) / 2.0,
                TOP + (HEIGHT - TOP - BOTTOM) / 2.0
            );
        }
        for (class, run, series) in [
            ("baseline", &baseline_chart, baseline_series),
            ("current", &current_chart, current_series),
        ] {
            let _ = writeln!(html, "<g class=\"{class}\">");
            render_line(html, run, &series.points, &axis, &scale);
            html.push_str("</g>\n");
        }
        html.push_str("</svg>\n");
        let _ = writeln!(
            html,
            "<figcaption>{}</figcaption>",
            escape(&overlay.caption)
        );
        for (label, run, series) in [
            ("Baseline", &baseline_chart, baseline_series),
            ("Current", &current_chart, current_series),
        ] {
            render_run_table(html, label, run, &series.points);
        }
        html.push_str("</figure>\n");
    }

    fn render_run_table(html: &mut String, label: &str, chart: &Chart, points: &[TimeSeriesPoint]) {
        let _ = write!(
            html,
            "<details>\n<summary>{label} data table ({} interval{})</summary>\n\
             <div class=\"scroll\"><table>\n\
             <thead><tr><th scope=\"col\">Start</th><th scope=\"col\">End</th>\
             <th scope=\"col\" class=\"num\">{} ({})</th></tr></thead>\n<tbody>\n",
            points.len(),
            if points.len() == 1 { "" } else { "s" },
            escape(&chart.title),
            escape(chart.unit)
        );
        for (point, value) in points.iter().zip(&chart.values) {
            let cell = match value {
                Some(value) => format_number(*value),
                None => "no replies".to_string(),
            };
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td>{}</td><td class=\"num\">{}</td></tr>",
                escape(&format_secs(point.start_secs)),
                escape(&format_secs(point.start_secs + point.duration_secs)),
                escape(&cell)
            );
        }
        html.push_str("</tbody></table></div>\n</details>\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AggregatedMetrics, LatencyStats};
    use crate::report::{PhaseTarget, SloCheck, SloEvaluation};
    use crate::timeseries::{IntervalLatency, TimeSeries};
    use std::borrow::Cow;
    use std::time::Duration;

    fn metrics_with(points: Vec<TimeSeriesPoint>) -> AggregatedMetrics {
        let mut metrics =
            AggregatedMetrics::merge(Vec::new(), Duration::from_secs(10), 0).expect("merge");
        metrics.time_series = TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        };
        metrics
    }

    fn point(i: usize, value: u64) -> TimeSeriesPoint {
        TimeSeriesPoint {
            start_secs: i as f64,
            duration_secs: 1.0,
            active_connections: value,
            messages_sent: value * 10,
            messages_recv: value * 10,
            errors: value % 3,
            throughput_msg_per_sec: (value * 10) as f64,
            latency: IntervalLatency {
                count: value,
                p50_us: value * 100,
                p90_us: value * 200,
                p99_us: value * 300,
                max_us: value * 400,
            },
            ..TimeSeriesPoint::default()
        }
    }

    fn view<'a>(
        metrics: &'a AggregatedMetrics,
        slo: &'a SloEvaluation,
        phases: &'a [PhaseTarget],
        target: &'a str,
    ) -> ReportView<'a> {
        ReportView {
            target_url: target,
            connections: 4,
            mode: Cow::Borrowed("Echo"),
            rate_per_conn: Some(10),
            stages: Vec::new(),
            phase_targets: phases,
            phases: &[],
            timestamp: Some(("Finished", "2026-10-25T10:00:00.000Z".into())),
            metrics,
            slo,
        }
    }

    fn no_slo() -> SloEvaluation {
        SloEvaluation {
            passed: true,
            checks: Vec::new(),
        }
    }

    /// Structural checks every rendered page must pass.
    fn assert_well_formed(html: &str) {
        assert!(
            html.starts_with("<!doctype html>\n<html lang=\"en\">"),
            "{html}"
        );
        assert!(html.trim_end().ends_with("</html>"));
        assert_eq!(html.matches("<svg").count(), html.matches("</svg>").count());
        assert!(!html.contains("<script"), "no script allowed");
        for external in ["http://", "https://", "@import", "url("] {
            assert!(!html.contains(external), "found {external}");
        }
        assert!(!html.contains("NaN") && !html.contains("inf"), "{html}");
        assert!(!html.contains('\u{2014}'), "no em dash");
    }

    fn svg_count(html: &str) -> usize {
        html.matches("<svg class=\"chart\"").count()
    }

    #[test]
    fn page_is_self_contained_with_one_svg_per_chart() {
        let metrics = metrics_with((0..25).map(|i| point(i, 5 + i as u64)).collect());
        let slo = no_slo();
        let html = render_html_report(&view(&metrics, &slo, &[], "ws://127.0.0.1:9001/"));
        assert_well_formed(&html);
        assert_eq!(svg_count(&html), 4);
        assert_eq!(html.matches("<title id=\"chart-").count(), 4);
        assert_eq!(html.matches("<desc id=\"chart-").count(), 4);
        assert_eq!(html.matches("<details>").count(), 4);
        assert!(html.contains("prefers-color-scheme: dark"));
        assert!(html.contains("No SLO thresholds were set"));
        assert!(html.contains("2026-10-25 10:00:00 UTC"));
        assert!(html.contains("10 msg/s per connection"));
    }

    #[test]
    fn charts_handle_zero_one_and_six_hundred_points() {
        let slo = no_slo();
        for count in [0usize, 1, 2, 600] {
            let metrics = metrics_with((0..count).map(|i| point(i, i as u64 % 7)).collect());
            let html = render_html_report(&view(&metrics, &slo, &[], "ws://h/"));
            assert_well_formed(&html);
            if count == 0 {
                assert_eq!(svg_count(&html), 0);
                assert!(html.contains("no time series"));
                continue;
            }
            assert_eq!(svg_count(&html), 4, "{count} points");
            assert_eq!(
                html.matches("<rect class=\"hit\"").count(),
                4 * count,
                "{count} points"
            );
            assert_eq!(
                html.matches("<tr><td>").count(),
                4 * count,
                "one table row per point and chart"
            );
        }

        // A single point with a value is drawn as a dot, not a path.
        let metrics = metrics_with(vec![point(0, 3)]);
        let html = render_html_report(&view(&metrics, &slo, &[], "ws://h/"));
        assert!(html.contains("<circle class=\"dot\""));
        assert!(!html.contains("<path class=\"line\""));
    }

    #[test]
    fn all_zero_series_and_gaps_render_without_dividing_by_zero() {
        let slo = no_slo();
        let zero = metrics_with((0..10).map(|i| point(i, 0)).collect());
        let html = render_html_report(&view(&zero, &slo, &[], "ws://h/"));
        assert_well_formed(&html);
        assert!(html.contains("Every value is zero."));
        assert!(
            html.contains("No data in any interval"),
            "latency has no replies"
        );
        let errors_svg = &html[html.find("id=\"chart-errors-title\"").unwrap()..];
        let errors_svg = &errors_svg[..errors_svg.find("</svg>").unwrap()];
        assert!(
            !errors_svg.contains(">0.5<"),
            "counts have whole-number ticks"
        );
        assert!(errors_svg.contains(">1</text>"));

        // Zero-length intervals at one instant, a pause between phases, and
        // an interval without replies in the middle of the latency line.
        let mut points: Vec<TimeSeriesPoint> = (0..6).map(|i| point(i, 4)).collect();
        points[2].latency = IntervalLatency::default();
        for p in &mut points[4..] {
            p.start_secs += 10.0;
        }
        points.push(TimeSeriesPoint {
            start_secs: 16.0,
            duration_secs: 0.0,
            ..TimeSeriesPoint::default()
        });
        let gappy = metrics_with(points);
        let html = render_html_report(&view(&gappy, &slo, &[], "ws://h/"));
        assert_well_formed(&html);
        let latency_svg = &html[html.find("id=\"chart-latency-title\"").unwrap()..];
        let latency_svg = &latency_svg[..latency_svg.find("</svg>").unwrap()];
        let path = latency_svg
            .split("<path class=\"line\" d=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("latency path");
        // Points 0-1, 3 alone (a dot), 4-5: two subpaths and one dot.
        assert_eq!(path.matches('M').count(), 2, "{path}");
        assert_eq!(latency_svg.matches("<circle").count(), 1);
        assert!(html.contains("no replies"));

        // Degenerate time: every point at the same instant.
        let instant = metrics_with(vec![TimeSeriesPoint::default(), TimeSeriesPoint::default()]);
        assert_well_formed(&render_html_report(&view(&instant, &slo, &[], "ws://h/")));
    }

    #[test]
    fn latency_spanning_orders_of_magnitude_uses_a_log_scale() {
        let mut points: Vec<TimeSeriesPoint> = (0..10).map(|i| point(i, 5)).collect();
        points[4].latency.p99_us = 900_000;
        let metrics = metrics_with(points);
        let slo = no_slo();
        let html = render_html_report(&view(&metrics, &slo, &[], "ws://h/"));
        assert!(html.contains("(ms, log scale)"), "{html}");
        assert_eq!(html.matches("log scale").count(), 1);
    }

    #[test]
    fn distributed_runs_list_their_agents_escaped() {
        let mut metrics = metrics_with((0..3).map(|i| point(i, 2)).collect());
        let slo = SloEvaluation {
            passed: true,
            checks: Vec::new(),
        };
        let target = "ws://h.test/".to_string();
        let html = render_html_report(&view(&metrics, &slo, &[], &target));
        assert!(!html.contains("<dt>Agents</dt>"), "{html}");

        metrics.distributed = Some(crate::metrics::DistributedSummary {
            agents: vec!["a.test:7700".into(), "<b>evil</b>:1".into()],
            lost_agents: vec!["<b>evil</b>:1".into()],
            ..Default::default()
        });
        let html = render_html_report(&view(&metrics, &slo, &[], &target));
        assert_well_formed(&html);
        assert!(
            html.contains("<dt>Agents</dt><dd>2 (lost: &lt;b&gt;evil&lt;/b&gt;:1)</dd>"),
            "{html}"
        );
    }

    #[test]
    fn report_strings_are_escaped() {
        let evil = "\"><script>alert(1)</script><img src=x onerror=alert(2)>";
        let metrics = metrics_with((0..3).map(|i| point(i, 2)).collect());
        let slo = SloEvaluation {
            passed: false,
            checks: vec![SloCheck {
                name: format!("p99 {evil}"),
                target: "<= 5ms".into(),
                actual: "<b>7ms</b>".into(),
                passed: false,
            }],
        };
        let phases = [PhaseTarget {
            phase: format!("warm{evil}up & 'go'"),
            target_url: url::Url::parse("wss://h.test/a?q=%3Cscript%3E").unwrap(),
        }];
        let target = format!("ws://h.test/{evil}");
        for phases in [&phases[..], &[]] {
            let mut v = view(&metrics, &slo, phases, &target);
            v.mode = Cow::Owned(format!("Echo{evil}"));
            v.stages = vec![crate::report::StageReport {
                duration: std::time::Duration::from_secs(3),
                target: 30,
            }];
            v.timestamp = Some(("Saved", evil.to_string()));
            let html = render_html_report(&v);
            assert_well_formed(&html);
            assert!(!html.contains("<img"), "{html}");
            assert!(!html.contains("<b>"), "{html}");
            assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
            assert!(html.contains("&quot;&gt;"));
            assert!(html.contains("FAIL: 1 of 1 SLO checks failed"));
            if !phases.is_empty() {
                assert!(html.contains("warm&quot;&gt;&lt;script&gt;"));
                assert!(html.contains("up &amp; &#39;go&#39;"));
            }
        }
    }

    #[test]
    fn phases_are_listed_with_their_own_slo_checks() {
        let metrics = metrics_with((0..3).map(|i| point(i, 2)).collect());
        let slo = no_slo();
        let phase = |name: &str, checks: Vec<SloCheck>| crate::report::PhaseReport {
            name: name.to_string(),
            target_url: url::Url::parse("ws://phase.test/").unwrap(),
            connections: 7,
            stages: Vec::new(),
            metrics: metrics.clone(),
            slo: SloEvaluation {
                passed: checks.iter().all(|check| check.passed),
                checks,
            },
        };
        let phases = [
            phase("warm<up>", Vec::new()),
            phase(
                "spike",
                vec![SloCheck {
                    name: "Latency p99".into(),
                    target: "<= 1ms".into(),
                    actual: "3ms".into(),
                    passed: false,
                }],
            ),
        ];
        let mut v = view(&metrics, &slo, &[], "ws://h/");
        assert!(!render_html_report(&v).contains("<h2>Phases</h2>"));
        v.phases = &phases;
        let html = render_html_report(&v);
        assert_well_formed(&html);
        assert!(html.contains("<h2>Phases</h2>"), "{html}");
        assert!(
            html.contains("<th scope=\"row\">warm&lt;up&gt;</th>"),
            "{html}"
        );
        assert!(html.contains("7 connections"), "{html}");
        assert!(html.contains("No SLO"), "{html}");
        assert!(html.contains("Phase spike SLO checks"), "{html}");
        assert!(!html.contains("Phase warm&lt;up&gt; SLO checks"), "{html}");
        assert!(html.contains("<code>3ms</code>"), "{html}");
    }

    #[test]
    fn missing_distributions_and_breakdown_render_as_absent() {
        let mut metrics = metrics_with(Vec::new());
        metrics.handshake_latency = LatencyStats::default();
        metrics
            .error_breakdown
            .insert(crate::error::ErrorCategory::Timeout, 3);
        metrics
            .error_breakdown
            .insert(crate::error::ErrorCategory::TcpConnect, 9);
        let slo = no_slo();
        let html = render_html_report(&view(&metrics, &slo, &[], "ws://h/"));
        assert_well_formed(&html);
        assert!(html.contains("n/a"));
        assert!(html.contains("RTT p99</p><p class=\"value\">no samples"));
        assert!(!html.contains("Response Time (from schedule)"));
        let tcp = html.find("tcp_connect").unwrap();
        let timeout = html.find("<code>timeout</code>").unwrap();
        assert!(tcp < timeout, "sorted by count");
    }

    #[test]
    fn nice_steps_and_ticks() {
        assert_eq!(nice_step(0.0, 4.0), 1.0);
        assert_eq!(nice_step(f64::NAN, 4.0), 1.0);
        assert_eq!(nice_step(400.0, 4.0), 100.0);
        assert_eq!(nice_step(7.0, 4.0), 2.0);
        assert_eq!(format_tick(0.0), "0");
        assert_eq!(format_tick(0.25), "0.25");
        assert_eq!(format_tick(2.5), "2.5");
        assert_eq!(format_tick(400.0), "400");
        assert_eq!(format_tick(15_000.0), "15k");
        assert_eq!(format_tick(2_000_000.0), "2M");
        assert_eq!(
            readable_timestamp("2026-10-25T14:03:07.123Z"),
            "2026-10-25 14:03:07 UTC"
        );
        assert_eq!(readable_timestamp("yesterday"), "yesterday");
    }

    #[cfg(feature = "compare")]
    mod comparison {
        use super::{assert_well_formed, metrics_with, point, svg_count};
        use crate::cli::OutputFormat;
        use crate::metrics::AggregatedMetrics;
        use crate::report::{SavedComparison, compare_metrics_in, render_comparison_view};
        use crate::series_compare::CompareWindow;
        use crate::timeseries::TimeSeriesPoint;

        fn stretched(
            points: usize,
            secs: f64,
            value: impl Fn(usize) -> u64,
        ) -> Vec<TimeSeriesPoint> {
            (0..points)
                .map(|i| TimeSeriesPoint {
                    start_secs: i as f64 * secs,
                    duration_secs: secs,
                    ..point(i, value(i))
                })
                .collect()
        }

        fn page(
            baseline: &AggregatedMetrics,
            current: &AggregatedMetrics,
            window: Option<&CompareWindow>,
            files: Option<(String, String)>,
        ) -> String {
            let comparison = compare_metrics_in(baseline, current, 10.0, window);
            render_comparison_view(&comparison.view(10.0, files), OutputFormat::Html)
        }

        #[test]
        fn page_lists_every_compared_metric_and_highlights_regressions() {
            let baseline = metrics_with(stretched(20, 1.0, |_| 10));
            let mut current = metrics_with(stretched(20, 1.0, |i| if i == 12 { 40 } else { 10 }));
            current.throughput_msg_per_sec = 50.0;
            let window = CompareWindow::parse("5s..15s").unwrap();
            let comparison = compare_metrics_in(&baseline, &current, 10.0, Some(&window));
            let html = render_comparison_view(
                &comparison.view(10.0, Some(("base.json".into(), "cur.json".into()))),
                OutputFormat::Html,
            );
            assert_well_formed(&html);
            assert!(html.contains("<title>wsblast baseline comparison</title>"));
            assert!(
                html.contains("<code>base.json</code>") && html.contains("<code>cur.json</code>")
            );
            assert!(html.contains("<dt>Window</dt><dd><code>5s..15s</code>"));
            assert!(html.contains("prefers-color-scheme: dark"));

            let regressed = comparison.regressions.len();
            assert!(regressed > 0);
            assert!(html.contains("banner fail"));
            assert!(
                html.contains(&format!("REGRESSION: {regressed} metrics regressed")),
                "{html}"
            );
            assert!(html.contains("exits with code 2"));
            assert_eq!(html.matches("<tr class=\"regressed\">").count(), regressed);
            assert_eq!(
                html.matches("<tr class=\"improved\">").count(),
                comparison.improvements.len()
            );
            let rows = html.matches("<th scope=\"row\"><code>").count();
            assert_eq!(rows, comparison.compared.len());
            assert!(
                rows > regressed + comparison.improvements.len(),
                "unchanged rows are listed"
            );
            // Regressions come first.
            let first_row = html.find("<th scope=\"row\">").unwrap();
            assert!(html[..first_row].ends_with("<tr class=\"regressed\">"));
            assert!(html.contains("<td>higher</td>") && html.contains("<td>lower</td>"));

            assert_eq!(svg_count(&html), 2, "throughput and p99 overlays");
            assert_eq!(html.matches("<rect class=\"window\"").count(), 2);
            assert_eq!(html.matches("<g class=\"baseline\">").count(), 2);
            assert_eq!(html.matches("<g class=\"current\">").count(), 2);
        }

        #[test]
        fn an_unchanged_comparison_passes() {
            let metrics = metrics_with(stretched(5, 1.0, |_| 10));
            let html = page(&metrics, &metrics, None, None);
            assert_well_formed(&html);
            assert!(html.contains("banner pass"));
            assert!(html.contains("No regressions: no metric changed by more than \u{b1}10.0%"));
            assert!(html.contains("exits with code 0"));
            assert!(!html.contains("class=\"regressed\"") && !html.contains("<dt>Window"));
            assert!(!html.contains("<rect class=\"window\""));
        }

        #[test]
        fn overlay_charts_take_runs_with_different_interval_counts() {
            let baseline = metrics_with(stretched(60, 1.0, |_| 10));
            let mut current = metrics_with(stretched(25, 2.0, |i| 5 + i as u64));
            current.time_series.interval_secs = 2.0;
            let html = page(&baseline, &current, None, None);
            assert_well_formed(&html);
            assert!(html.contains("Baseline: 60 intervals of 1s; current: 25 intervals of 2s"));
            assert!(html.contains("Baseline data table (60 intervals)"));
            assert!(html.contains("Current data table (25 intervals)"));
            // One shared time axis to the end of the longer run.
            assert!(html.contains(">60s</text>"), "{html}");
            // Each run is drawn as its own line in each chart.
            for class in ["baseline", "current"] {
                let groups: Vec<&str> = html
                    .split(&format!("<g class=\"{class}\">"))
                    .skip(1)
                    .map(|rest| &rest[..rest.find("</g>").unwrap()])
                    .collect();
                assert_eq!(groups.len(), 2);
                assert!(
                    groups
                        .iter()
                        .all(|group| group.contains("<path class=\"line\""))
                );
            }
            let current_path = html
                .split("<g class=\"current\">")
                .nth(1)
                .unwrap()
                .split("</g>")
                .next()
                .unwrap();
            assert_eq!(current_path.matches('L').count(), 24, "{current_path}");
        }

        #[test]
        fn reports_without_a_time_series_have_no_charts() {
            let legacy = metrics_with(Vec::new());
            let with_series = metrics_with(stretched(5, 1.0, |_| 10));
            let html = page(&legacy, &legacy, None, None);
            assert_well_formed(&html);
            assert_eq!(svg_count(&html), 0);
            assert!(
                html.contains("the baseline and the current reports have none"),
                "{html}"
            );
            let html = page(&with_series, &legacy, None, None);
            assert_eq!(svg_count(&html), 0);
            assert!(html.contains("the current report has none"), "{html}");
        }

        #[test]
        fn strings_from_files_are_escaped() {
            let metrics = metrics_with(Vec::new());
            let html = page(
                &metrics,
                &metrics,
                None,
                Some((
                    "<script>a</script>.json".into(),
                    "\"cur\" & 'x'.json".into(),
                )),
            );
            assert_well_formed(&html);
            assert!(html.contains("&lt;script&gt;a&lt;/script&gt;.json"));
            assert!(html.contains("&quot;cur&quot; &amp; &#39;x&#39;.json"));

            let saved = SavedComparison::detect(
                r#"{"schema_version":"1.2.0","threshold":5,"window":{"start_secs":10},
                    "regressions":[{"metric":"errors.<img src=x>","baseline":0,"current":2,"pct_change":100}],
                    "improvements":[{"metric":"throughput_msg_per_sec","baseline":10,"current":20,"pct_change":100}]}"#,
            )
            .unwrap()
            .unwrap();
            let html = render_comparison_view(
                &saved.view(Some((
                    "Comparison saved",
                    "2026-10-25T10:00:00.000Z".into(),
                ))),
                OutputFormat::Html,
            );
            assert_well_formed(&html);
            assert!(!html.contains("<img"), "{html}");
            assert!(html.contains("<code>errors.&lt;img src=x&gt;</code>"));
            assert!(html.contains("Comparison JSON, schema 1.2.0"));
            assert!(html.contains("2026-10-25 10:00:00 UTC"));
            assert!(html.contains("<code>10s..</code>"));
            assert!(html.contains("records only the metrics that moved"));
            assert!(html.contains("does not include the time series"));
            assert_eq!(svg_count(&html), 0);
            assert_eq!(html.matches("<th scope=\"row\"><code>").count(), 2);
            assert!(!html.contains("Metrics compared"));
        }
    }
}
