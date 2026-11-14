//! Every colour and style the TUI draws with.

use crate::ipc::protocol::DownloadStatus;
use ratatui::style::{Color, Modifier, Style};

/// The `rustdm` badge and the selected tab.
pub const BRAND: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);
/// Highlights: total speed, the inspector border, the speed graph.
pub const ACCENT: Style = Style::new().fg(Color::Cyan);
/// The screen being shown, in the header.
pub const CURRENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
/// Labels and secondary text.
pub const MUTED: Style = Style::new().fg(Color::Gray);
/// Hints and borders that should recede.
pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const ERROR: Style = Style::new().fg(Color::Red);
pub const SUCCESS: Style = Style::new().fg(Color::Green);
/// Something in progress: an active connection, a write cursor.
pub const WORKING: Style = Style::new().fg(Color::Yellow);
pub const TABLE_HEADER: Style = Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD);
pub const SELECTED_ROW: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::BOLD);
pub const BUTTON: Style = Style::new().fg(Color::Black).bg(Color::Gray);
pub const BUTTON_ADD: Style = Style::new().fg(Color::Black).bg(Color::Green);
pub const BUTTON_QUIT: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
/// A text field that does not have the cursor.
pub const FIELD: Style = Style::new().fg(Color::Gray).bg(Color::Black);
/// The text field being typed in.
pub const FIELD_FOCUSED: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
/// Laid over the screen behind an open dialog.
pub const BEHIND_DIALOG: Style = Style::new().add_modifier(Modifier::DIM);
/// A prompt waiting for input.
pub const PROMPT: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
/// A question that needs an answer before anything else happens.
pub const CONFIRM: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// An active download that has had no data for a while.
pub const STALLED: Style = Style::new().fg(Color::LightRed);

pub fn status_style(status: DownloadStatus) -> Style {
    let color = match status {
        DownloadStatus::Active => Color::Cyan,
        DownloadStatus::Queued => Color::Yellow,
        DownloadStatus::Pausing | DownloadStatus::Paused => Color::Gray,
        DownloadStatus::Retrying => Color::Magenta,
        DownloadStatus::Completed => Color::Green,
        DownloadStatus::Failed => Color::Red,
        DownloadStatus::Cancelled => Color::DarkGray,
    };
    Style::new().fg(color)
}
