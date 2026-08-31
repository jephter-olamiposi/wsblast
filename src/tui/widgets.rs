//! TUI layout and widget compositions.

use crate::config::LoadTestConfig;
use crate::metrics::LiveSnapshot;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Sparkline};

pub fn render_dashboard(
    f: &mut Frame,
    config: &LoadTestConfig,
    snap: &LiveSnapshot,
    progress_ratio: f64,
    rps_history: &[u64],
    elapsed_secs: f64,
) {
    let size = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(size);

    render_header(f, chunks[0], config, elapsed_secs);
    render_progress(
        f,
        chunks[1],
        progress_ratio,
        elapsed_secs,
        config.duration.as_secs_f64(),
    );
    render_metric_cards(f, chunks[2], config, snap, elapsed_secs);
    render_sparkline(f, chunks[3], rps_history);
    render_footer(f, chunks[4]);
}

fn render_header(f: &mut Frame, area: Rect, config: &LoadTestConfig, elapsed_secs: f64) {
    let title_line = Line::from(vec![
        Span::styled(
            " wsblast ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            config.target_url.as_str(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  |  "),
        Span::styled("Mode: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:?}", config.mode),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("Elapsed: {:.1}s", elapsed_secs),
            Style::default().fg(Color::Green),
        ),
    ]);

    let p = Paragraph::new(title_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(p, area);
}

fn render_progress(f: &mut Frame, area: Rect, ratio: f64, elapsed: f64, total: f64) {
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Test Execution Progress ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(format!(
            "{:.1}% ({:.1}s / {:.1}s)",
            ratio * 100.0,
            elapsed,
            total
        ));

    f.render_widget(gauge, area);
}

fn render_metric_cards(
    f: &mut Frame,
    area: Rect,
    config: &LoadTestConfig,
    snap: &LiveSnapshot,
    elapsed: f64,
) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let rps = if elapsed > 0.0 {
        snap.messages_received as f64 / elapsed
    } else {
        0.0
    };

    let mb_recv = snap.bytes_received as f64 / (1024.0 * 1024.0);

    let conns_p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                format!("{}", snap.active_connections),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" / {}", config.connections)),
        ]),
        Line::from(format!("Failed: {}", snap.connections_failed)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Active Connections ")
            .border_style(Style::default().fg(Color::Blue)),
    );
    f.render_widget(conns_p, cards[0]);

    let tput_p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                format!("{:.1}", rps),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" msg/s"),
        ]),
        Line::from(format!("Total: {}", snap.messages_received)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Inbound Throughput ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(tput_p, cards[1]);

    let bw_p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                format!("{:.2}", mb_recv),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" MB"),
        ]),
        Line::from(format!(
            "Sent: {:.2} MB",
            snap.bytes_sent as f64 / (1024.0 * 1024.0)
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Data Transferred ")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(bw_p, cards[2]);

    let err_color = if snap.total_errors > 0 {
        Color::Red
    } else {
        Color::Green
    };
    let err_p = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                format!("{}", snap.total_errors),
                Style::default().fg(err_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" failures"),
        ]),
        Line::from(if snap.total_errors == 0 {
            "All healthy"
        } else {
            "Review taxonomy"
        }),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Total Errors ")
            .border_style(Style::default().fg(err_color)),
    );
    f.render_widget(err_p, cards[3]);
}

fn render_sparkline(f: &mut Frame, area: Rect, history: &[u64]) {
    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Real-Time Throughput Trend (msg/sec) ")
                .border_style(Style::default().fg(Color::Magenta)),
        )
        .data(history)
        .style(Style::default().fg(Color::Magenta));

    f.render_widget(sparkline, area);
}

fn render_footer(f: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " [Q] ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Quit & Report  "),
        Span::styled(
            " [Ctrl+C] ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Abort Test  "),
    ]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::NONE));

    f.render_widget(footer, area);
}
