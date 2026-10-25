//! TUI layout and widget compositions.
//!
//! Everything is drawn from a [`DashboardView`], so a frame can be rendered
//! to any backend, including ratatui's `TestBackend` in the tests below.

use super::model::{
    DashboardView, SloScope, SloState, basis_label, format_count, format_duration, format_micros,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Sparkline};

/// Below this size the panels no longer fit and the compact view is drawn.
const FULL_MIN_WIDTH: u16 = 72;
const FULL_MIN_HEIGHT: u16 = 22;

/// Draws the whole dashboard for one frame.
pub(super) fn render_dashboard(f: &mut Frame, view: &DashboardView) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < FULL_MIN_WIDTH || area.height < FULL_MIN_HEIGHT {
        render_compact(f, area, view);
        return;
    }

    let scenario = view
        .phase
        .as_ref()
        .is_some_and(|phase| phase.name.is_some());
    // The SLO panel and the phase list share a row; it grows with them up
    // to what the terminal leaves after the fixed rows.
    let list_rows = view
        .slo
        .len()
        .max(if scenario { view.finished.len() + 1 } else { 0 })
        .max(1);
    let spare = area.height.saturating_sub(1 + 1 + 7 + 5 + 1);
    let middle = (list_rows as u16 + 2).clamp(3, spare.max(3));

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(7),
            Constraint::Length(middle),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(f, rows[0], view);
    render_progress(f, rows[1], view);
    render_cards(f, rows[2], view);
    if scenario {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(rows[3]);
        render_slo(f, halves[0], view);
        render_phases(f, halves[1], view);
    } else {
        render_slo(f, rows[3], view);
    }
    render_sparklines(f, rows[4], view);
    render_footer(f, rows[5]);
}

fn panel(title: String, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(color))
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn slo_color(state: SloState) -> Color {
    match state {
        SloState::Ok => Color::Green,
        SloState::Breaching => Color::Red,
        SloState::NoData => Color::DarkGray,
    }
}

fn slo_word(state: SloState) -> &'static str {
    match state {
        SloState::Ok => "ok",
        SloState::Breaching => "BREACH",
        SloState::NoData => "-",
    }
}

fn phase_label(view: &DashboardView) -> Option<String> {
    let phase = view.phase.as_ref()?;
    let name = phase.name.as_ref()?;
    Some(format!("phase {}/{} {name}", phase.number, phase.total))
}

fn render_header(f: &mut Frame, area: Rect, view: &DashboardView) {
    let mut spans = vec![
        Span::styled(
            " wsblast ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(view.info.target.clone(), bold(Color::White)),
        Span::styled("  mode ", dim()),
        Span::styled(view.info.mode.clone(), Style::default().fg(Color::Yellow)),
        Span::styled("  elapsed ", dim()),
        Span::styled(
            format_duration(view.run_elapsed),
            Style::default().fg(Color::Green),
        ),
    ];
    if let Some(label) = phase_label(view) {
        spans.push(Span::styled("  ", dim()));
        spans.push(Span::styled(label, bold(Color::Magenta)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_progress(f: &mut Frame, area: Rect, view: &DashboardView) {
    let (ratio, label) = match &view.phase {
        None => (0.0, "connecting".to_string()),
        Some(phase) if phase.starting => (0.0, "starting phase".to_string()),
        Some(phase) => (
            phase.ratio(),
            format!(
                "{:.0}%  {} / {}",
                phase.ratio() * 100.0,
                format_duration(phase.elapsed),
                format_duration(phase.duration)
            ),
        ),
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

fn render_cards(f: &mut Frame, area: Rect, view: &DashboardView) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(area);
    render_latency(f, cards[0], view);
    render_connections(f, cards[1], view);
    render_traffic(f, cards[2], view);
}

fn render_latency(f: &mut Frame, area: Rect, view: &DashboardView) {
    let latency = &view.latency;
    let value = |us: u64| {
        if latency.has_samples() {
            Span::styled(format!("{:>9}", format_micros(us)), bold(Color::Cyan))
        } else {
            Span::styled(format!("{:>9}", "-"), dim())
        }
    };
    let note = match latency.samples {
        None => "first interval pending".to_string(),
        Some(0) => "no replies in interval".to_string(),
        Some(count) => format!(
            "{} samples in {}",
            format_count(count as f64),
            format_duration(latency.interval)
        ),
    };
    let lines = vec![
        Line::from(vec![Span::raw("p50 "), value(latency.p50_us)]),
        Line::from(vec![Span::raw("p90 "), value(latency.p90_us)]),
        Line::from(vec![Span::raw("p99 "), value(latency.p99_us)]),
        Line::from(""),
        Line::from(Span::styled(note, dim())),
    ];
    // The percentiles are those of the last completed interval.
    let title = match latency.basis_label() {
        "response time" => " Response time ",
        "RTT" => " RTT ",
        _ => " Latency ",
    }
    .to_string();
    f.render_widget(Paragraph::new(lines).block(panel(title, Color::Cyan)), area);
}

fn render_connections(f: &mut Frame, area: Rect, view: &DashboardView) {
    let c = &view.counters;
    let mut open = vec![
        Span::raw("open    "),
        Span::styled(c.active_connections.to_string(), bold(Color::Green)),
    ];
    if let Some(target) = view.target_connections {
        open.push(Span::raw(format!(" / {target} target")));
    }
    let (stage, left) = match view.stage {
        Some(stage) => (
            Line::from(vec![
                Span::raw(format!("stage   {}/{} ", stage.number, stage.total)),
                Span::styled(
                    format!("-> {}", stage.end_target),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::raw("left    "),
                Span::styled(
                    format_duration(stage.left),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
        ),
        None => (Line::from(Span::styled("no stages", dim())), Line::from("")),
    };
    let not_open = view
        .target_connections
        .map(|target| (target as u64).saturating_sub(c.active_connections));
    let failed_style = if c.connections_failed > 0 {
        bold(Color::Red)
    } else {
        Style::default()
    };
    let lines = vec![
        Line::from(open),
        stage,
        left,
        Line::from(vec![
            Span::raw("failed  "),
            Span::styled(c.connections_failed.to_string(), failed_style),
            Span::styled(" handshakes", dim()),
        ]),
        Line::from(format!(
            "reconn  {}  down {}",
            c.reconnects,
            not_open.map_or_else(|| "-".to_string(), |n| n.to_string())
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).block(panel(" Connections ".to_string(), Color::Blue)),
        area,
    );
}

fn render_traffic(f: &mut Frame, area: Rect, view: &DashboardView) {
    let c = &view.counters;
    let w = &view.window;
    let window = format_duration(std::time::Duration::from_secs_f64(w.secs.max(0.0)));
    let error_style = if w.errors > 0 {
        bold(Color::Red)
    } else {
        bold(Color::Green)
    };
    let lines = vec![
        Line::from(vec![
            Span::raw("msg/s   "),
            Span::styled(format_count(w.throughput), bold(Color::Magenta)),
        ]),
        Line::from(vec![
            Span::raw("errors  "),
            Span::styled(w.errors.to_string(), error_style),
            Span::raw(format!(
                "  ~{}",
                w.error_rate
                    .map_or_else(|| "-".to_string(), |r| format!("{:.2}%", r * 100.0))
            )),
        ]),
        Line::from(format!("total   {} errors", c.total_errors)),
        Line::from(format!("sent    {}", format_count(c.messages_sent as f64))),
        Line::from(format!(
            "recv    {}",
            format_count(c.messages_received as f64)
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).block(panel(format!(" Traffic (last {window}) "), Color::Magenta)),
        area,
    );
}

fn render_slo(f: &mut Frame, area: Rect, view: &DashboardView) {
    let scope = match view.slo_scope {
        SloScope::Run => "SLO",
        SloScope::Phase => "Phase SLO",
        SloScope::Scenario => "Scenario SLO",
    };
    let title = format!(" {scope} (live; the final report gates) ");
    let lines: Vec<Line> = if view.slo.is_empty() {
        vec![Line::from(Span::styled("no thresholds set", dim()))]
    } else {
        view.slo
            .iter()
            .map(|row| {
                Line::from(vec![
                    Span::raw(format!("{:<9}", row.name)),
                    Span::styled(format!("{:<11}", row.threshold), dim()),
                    Span::styled(format!("{:>10} ", row.live), bold(slo_color(row.state))),
                    Span::styled(slo_word(row.state), bold(slo_color(row.state))),
                ])
            })
            .collect()
    };
    let breaching = view.slo.iter().any(|row| row.state == SloState::Breaching);
    let color = if breaching { Color::Red } else { Color::Green };
    f.render_widget(Paragraph::new(lines).block(panel(title, color)), area);
}

fn render_phases(f: &mut Frame, area: Rect, view: &DashboardView) {
    let inner_rows = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = view
        .finished
        .iter()
        .map(|phase| {
            let (word, color) = if phase.passed {
                ("PASS", Color::Green)
            } else {
                ("FAIL", Color::Red)
            };
            let mut spans = vec![
                Span::styled(format!("{word} "), bold(color)),
                Span::raw(phase.name.clone()),
            ];
            if !phase.failed.is_empty() {
                spans.push(Span::styled(
                    format!(" ({})", phase.failed.join(", ")),
                    dim(),
                ));
            } else if phase.checks == 0 {
                spans.push(Span::styled(" (no SLO)", dim()));
            }
            Line::from(spans)
        })
        .collect();
    if let Some(phase) = &view.phase
        && let Some(name) = &phase.name
    {
        lines.push(Line::from(vec![
            Span::styled("NOW  ", bold(Color::Yellow)),
            Span::raw(name.clone()),
        ]));
    }
    // Keep the newest phases when the list is longer than the panel.
    let skip = lines.len().saturating_sub(inner_rows);
    let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
    f.render_widget(
        Paragraph::new(lines).block(panel(" Phases ".to_string(), Color::Yellow)),
        area,
    );
}

/// The newest points that fit a sparkline drawn in `area` with borders.
fn tail(data: &[u64], area: Rect) -> &[u64] {
    let width = area.width.saturating_sub(2) as usize;
    &data[data.len().saturating_sub(width)..]
}

fn render_sparklines(f: &mut Frame, area: Rect, view: &DashboardView) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let tput = Sparkline::default()
        .block(panel(" Throughput msg/s ".to_string(), Color::Magenta))
        .data(tail(&view.throughput_history, halves[0]))
        .style(Style::default().fg(Color::Magenta));
    f.render_widget(tput, halves[0]);

    let peak = view.p99_history.iter().copied().max().unwrap_or(0);
    let title = if peak > 0 {
        format!(
            " p99 {}, max {} ",
            view.latency.basis_label(),
            format_micros(peak)
        )
    } else {
        format!(" p99 {} ", view.latency.basis_label())
    };
    let p99 = Sparkline::default()
        .block(panel(title, Color::Cyan))
        .data(tail(&view.p99_history, halves[1]))
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(p99, halves[1]);
}

fn footer_line() -> Line<'static> {
    let key = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(" q ", key),
        Span::raw(" quit and report  "),
        Span::styled(" Ctrl+C ", key),
        Span::raw(" abort"),
    ])
}

fn render_footer(f: &mut Frame, area: Rect) {
    f.render_widget(Paragraph::new(footer_line()), area);
}

/// One line per topic, most important first, for terminals too small for
/// the panels. Lines that do not fit are cut off by the terminal edge.
fn render_compact(f: &mut Frame, area: Rect, view: &DashboardView) {
    let latency = &view.latency;
    let mut lines = Vec::new();

    let mut header = vec![
        Span::styled("wsblast ", bold(Color::Cyan)),
        Span::raw(format_duration(view.run_elapsed)),
    ];
    if let Some(label) = phase_label(view) {
        header.push(Span::styled(format!(" {label}"), bold(Color::Magenta)));
    }
    lines.push(Line::from(header));

    if let Some(phase) = &view.phase
        && !phase.starting
    {
        lines.push(Line::from(format!(
            "{:.0}% of {}",
            phase.ratio() * 100.0,
            format_duration(phase.duration)
        )));
    }

    let basis = basis_label(latency.basis);
    lines.push(if latency.has_samples() {
        Line::from(vec![
            Span::raw(format!("{basis} p99 ")),
            Span::styled(format_micros(latency.p99_us), bold(Color::Cyan)),
            Span::raw(format!(" p50 {}", format_micros(latency.p50_us))),
        ])
    } else {
        Line::from(Span::styled(format!("{basis}: no samples yet"), dim()))
    });

    let mut conns = format!("open {}", view.counters.active_connections);
    if let Some(target) = view.target_connections {
        conns.push_str(&format!("/{target}"));
    }
    if let Some(stage) = view.stage {
        conns.push_str(&format!(
            " stage {}/{} {} left",
            stage.number,
            stage.total,
            format_duration(stage.left)
        ));
    }
    lines.push(Line::from(conns));

    lines.push(Line::from(format!(
        "msg/s {} err {} ~{}",
        format_count(view.window.throughput),
        view.window.errors,
        view.window
            .error_rate
            .map_or_else(|| "-".to_string(), |r| format!("{:.2}%", r * 100.0))
    )));

    if !view.slo.is_empty() {
        let mut spans = vec![Span::raw("SLO ")];
        for row in &view.slo {
            spans.push(Span::styled(
                format!("{} {} ", row.name, slo_word(row.state)),
                Style::default().fg(slo_color(row.state)),
            ));
        }
        lines.push(Line::from(spans));
    }

    if !view.finished.is_empty() {
        let mut spans = vec![Span::raw("done ")];
        for phase in &view.finished {
            let (word, color) = if phase.passed {
                ("PASS", Color::Green)
            } else {
                ("FAIL", Color::Red)
            };
            spans.push(Span::raw(format!("{} ", phase.name)));
            spans.push(Span::styled(format!("{word} "), Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }

    // The keys stay on the last row.
    let body_rows = area.height.saturating_sub(1);
    let body = Rect {
        height: body_rows,
        ..area
    };
    lines.truncate(body_rows as usize);
    f.render_widget(Paragraph::new(lines), body);
    let footer = Rect {
        y: area.y + body_rows,
        height: 1,
        ..area
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[q]", bold(Color::White)),
            Span::raw(" quit "),
            Span::styled("[^C]", bold(Color::White)),
            Span::raw(" abort"),
        ])),
        footer,
    );
}
