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
pub(crate) use source::{PhasePlan, RunFeed, RunProgress};
