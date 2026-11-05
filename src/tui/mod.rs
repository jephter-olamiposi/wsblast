//! Interactive Terminal User Interface (TUI) powered by ratatui.
//!
//! [`TuiApp`] draws a [`DashboardSource`]; see [`source`] for what a source
//! provides and how a run other than a local one can drive the dashboard.

mod app;
mod model;
pub(crate) mod source;
mod widgets;

#[cfg(test)]
mod tests;

pub(crate) use app::TuiApp;
pub(crate) use model::RunInfo;
#[cfg(feature = "distributed")]
pub(crate) use source::AgentsView;
pub(crate) use source::{DashboardSource, PhasePlan, RunFeed, RunProgress};

/// Draws one frame of the dashboard of `source`, as it looks after
/// `refreshes` reads, on a `width` by `height` test terminal.
#[cfg(all(test, feature = "distributed"))]
pub(crate) fn render_test_frame(
    source: &dyn DashboardSource,
    info: RunInfo,
    refreshes: usize,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    let mut dashboard = model::Dashboard::new(info);
    let mut view = dashboard.update(source);
    for _ in 1..refreshes {
        view = dashboard.update(source);
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal
        .draw(|frame| widgets::render_dashboard(frame, &view))
        .expect("draw");
    terminal.backend().buffer().clone()
}
