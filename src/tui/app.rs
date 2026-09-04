//! Interactive TUI application lifecycle and terminal event loop.

use crate::config::LoadTestConfig;
use crate::metrics::LiveMetrics;
use crate::tui::widgets::render_dashboard;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub struct TuiApp {
    config: Arc<LoadTestConfig>,
    live_metrics: Arc<LiveMetrics>,
    cancel_token: CancellationToken,
    history: VecDeque<u64>,
    last_recv_count: u64,
    last_tick: Instant,
}

impl TuiApp {
    pub fn new(
        config: Arc<LoadTestConfig>,
        live_metrics: Arc<LiveMetrics>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            config,
            live_metrics,
            cancel_token,
            history: VecDeque::with_capacity(120),
            last_recv_count: 0,
            last_tick: Instant::now(),
        }
    }

    /// Initializes terminal raw mode, runs dashboard event loop, and guarantees terminal restoration on exit.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if terminal raw mode configuration or alternate screen transitions fail.
    pub async fn run(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.event_loop(&mut terminal).await;

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        res
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        let start_time = Instant::now();
        let total_duration = self.config.duration.as_secs_f64();

        while !self.cancel_token.is_cancelled() {
            let elapsed_secs = start_time.elapsed().as_secs_f64();
            let progress_ratio = if total_duration > 0.0 {
                elapsed_secs / total_duration
            } else {
                1.0
            };

            let now = Instant::now();
            let tick_delta = now.duration_since(self.last_tick).as_secs_f64();
            let snap = self.live_metrics.snapshot();

            if tick_delta >= 0.5 {
                let current_recv = snap.messages_received;
                let delta_msgs = current_recv.saturating_sub(self.last_recv_count);
                let instant_rps = (delta_msgs as f64 / tick_delta) as u64;

                if self.history.len() >= 100 {
                    self.history.pop_front();
                }
                self.history.push_back(instant_rps);
                self.last_recv_count = current_recv;
                self.last_tick = now;
            }

            let history_slice: Vec<u64> = self.history.iter().copied().collect();

            terminal.draw(|f| {
                render_dashboard(
                    f,
                    &self.config,
                    &snap,
                    progress_ratio,
                    &history_slice,
                    elapsed_secs,
                );
            })?;

            if event::poll(Duration::from_millis(80))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc, _) => {
                                self.cancel_token.cancel();
                                break;
                            }
                            (KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL) => {
                                self.cancel_token.cancel();
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }

            tokio::task::yield_now().await;
        }

        Ok(())
    }
}
