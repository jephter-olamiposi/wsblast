//! Interactive TUI application lifecycle and terminal event loop.

use crate::tui::model::{Dashboard, RunInfo};
use crate::tui::source::DashboardSource;
use crate::tui::widgets::render_dashboard;
use crossterm::cursor::Show;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often the dashboard reads its source and redraws: 5 Hz.
const REFRESH: Duration = Duration::from_millis(200);

/// Restores the terminal even when the dashboard panics mid-render.
struct TerminalGuard {
    active: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
    }
}

/// Live dashboard drawn while a run or scenario is in progress.
pub(crate) struct TuiApp {
    source: Arc<dyn DashboardSource>,
    dashboard: Dashboard,
    /// Cancelled by `q` or Ctrl+C to stop the run.
    cancel_token: CancellationToken,
    /// Cancelled by the caller once the run has returned.
    done: CancellationToken,
}

impl TuiApp {
    /// A dashboard of the run `source` reports on. `cancel_token` stops the
    /// run; the dashboard closes when it is cancelled or `done` is.
    pub(crate) fn new(
        info: RunInfo,
        source: Arc<dyn DashboardSource>,
        cancel_token: CancellationToken,
        done: CancellationToken,
    ) -> Self {
        Self {
            source,
            dashboard: Dashboard::new(info),
            cancel_token,
            done,
        }
    }

    /// Initializes terminal raw mode, runs dashboard event loop, and guarantees terminal restoration on exit.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if terminal raw mode configuration or alternate screen transitions fail.
    pub(crate) async fn run(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let mut guard = TerminalGuard { active: true };
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.event_loop(&mut terminal).await;

        guard.active = false;
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        res
    }

    /// Redraws every [`REFRESH`] and handles keys as they arrive. Neither
    /// blocks a runtime thread: the terminal's events are read as a stream.
    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        let mut events = EventStream::new();
        let mut refresh = tokio::time::interval(REFRESH);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut input_open = true;

        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => break,
                _ = self.done.cancelled() => break,
                _ = refresh.tick() => {
                    let view = self.dashboard.update(self.source.as_ref());
                    terminal.draw(|f| render_dashboard(f, &view))?;
                }
                event = events.next(), if input_open => match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if is_stop_key(key.code, key.modifiers) {
                            self.cancel_token.cancel();
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err),
                    // The terminal closed its input; keep drawing until
                    // the run ends.
                    None => input_open = false,
                },
            }
        }

        Ok(())
    }
}

/// `q`, `Esc` and Ctrl+C stop the run; the report follows either way.
fn is_stop_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => true,
        KeyCode::Char('c') => modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_and_abort_keys_stop_the_run() {
        assert!(is_stop_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(is_stop_key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(is_stop_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!is_stop_key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!is_stop_key(KeyCode::Char('x'), KeyModifiers::NONE));
    }
}
