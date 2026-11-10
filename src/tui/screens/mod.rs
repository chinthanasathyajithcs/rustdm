//! The TUI's screens. Each reads and changes `App` and returns an `Action`;
//! none of them talks to the daemon.

pub mod downloads;

use ratatui::layout::{Position, Rect};

/// The value whose area contains `at`, if any.
pub(crate) fn hit<T: Clone>(areas: &[(Rect, T)], at: Position) -> Option<T> {
    areas
        .iter()
        .find(|(rect, _)| rect.contains(at))
        .map(|(_, value)| value.clone())
}

/// Draw the current screen and record what is clickable in `app.hits`.
pub fn render(frame: &mut ratatui::Frame, app: &mut crate::tui::state::App) {
    downloads::render(frame, app);
}
