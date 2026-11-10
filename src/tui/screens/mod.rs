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
