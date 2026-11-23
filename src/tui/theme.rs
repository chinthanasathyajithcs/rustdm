//! Every colour and style the TUI draws with: the Ember look, warm greys with
//! an amber accent. Colours are 24-bit, so they look the same in every terminal
//! that has true colour (the Windows console has since Windows 10). Plain text
//! keeps the terminal's own colour and nothing paints the background.

use crate::ipc::protocol::DownloadStatus;
use ratatui::style::{Color, Modifier, Style};

/// The Ember palette. Styles below are built only from these.
pub mod palette {
    use ratatui::style::Color;

    /// The accent: the badge, progress, the active status.
    pub const AMBER: Color = Color::Rgb(0xe5, 0xa0, 0x4f);
    /// Finished parts of the segment map.
    pub const AMBER_DEEP: Color = Color::Rgb(0xb9, 0x7c, 0x39);
    /// Where connections are writing.
    pub const AMBER_GLOW: Color = Color::Rgb(0xf2, 0xd4, 0x9b);
    /// The speed graph.
    pub const EMBER: Color = Color::Rgb(0xa8, 0x6f, 0x33);
    /// Text on an amber background.
    pub const ON_AMBER: Color = Color::Rgb(0x1a, 0x14, 0x0c);
    pub const BRIGHT: Color = Color::Rgb(0xf5, 0xef, 0xe4);
    pub const STONE: Color = Color::Rgb(0xb4, 0xac, 0x9f);
    pub const MUTED: Color = Color::Rgb(0x8e, 0x86, 0x78);
    pub const FAINT: Color = Color::Rgb(0x77, 0x6f, 0x63);
    pub const DUSK: Color = Color::Rgb(0x5c, 0x56, 0x4d);
    /// Panel borders.
    pub const FRAME: Color = Color::Rgb(0x4a, 0x43, 0x3a);
    /// Borders that should recede further.
    pub const RULE: Color = Color::Rgb(0x39, 0x34, 0x2d);
    /// The empty part of a progress bar.
    pub const TRACK: Color = Color::Rgb(0x33, 0x30, 0x2b);
    /// Missing parts of the segment map.
    pub const HOLLOW: Color = Color::Rgb(0x2a, 0x26, 0x21);
    pub const BUTTON_BG: Color = Color::Rgb(0x2c, 0x27, 0x21);
    pub const QUIT_BG: Color = Color::Rgb(0x20, 0x1c, 0x18);
    pub const FIELD_BG: Color = Color::Rgb(0x1f, 0x1b, 0x16);
    pub const SELECTED_BG: Color = Color::Rgb(0x2a, 0x22, 0x19);
    pub const SAGE: Color = Color::Rgb(0x9c, 0xbf, 0x78);
    pub const CORAL: Color = Color::Rgb(0xe0, 0x78, 0x56);
    pub const RED: Color = Color::Rgb(0xe0, 0x5a, 0x4f);
    pub const LAVENDER: Color = Color::Rgb(0xc9, 0x9a, 0xd6);
}

use palette::*;

const fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

const fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

/// The `rustdm` badge.
pub const BRAND: Style = Style::new()
    .fg(ON_AMBER)
    .bg(AMBER)
    .add_modifier(Modifier::BOLD);
/// Highlights: total speed, dialog borders, Help's keys.
pub const ACCENT: Style = fg(AMBER);
/// The screen being shown, in the header.
pub const CURRENT: Style = bold(AMBER);
/// The tab being shown above the table.
pub const TAB_SELECTED: Style = bold(AMBER).add_modifier(Modifier::UNDERLINED);
/// Labels and secondary text.
pub const MUTED: Style = fg(palette::MUTED);
/// Hints and separators that should recede.
pub const DIM: Style = fg(FAINT);
/// Borders of panels that are not the focus: the table, the logs.
pub const FRAME: Style = fg(RULE);
/// The border of the details pane.
pub const PANEL: Style = fg(palette::FRAME);
/// A panel's title.
pub const PANEL_TITLE: Style = bold(BRIGHT);
pub const ERROR: Style = fg(RED);
pub const SUCCESS: Style = fg(SAGE);
/// Something in progress: an active connection.
pub const WORKING: Style = fg(AMBER);
/// The filled part of a progress bar.
pub const BAR: Style = fg(AMBER);
/// The empty part of a progress bar.
pub const BAR_TRACK: Style = fg(TRACK);
pub const SEGMENT_DONE: Style = fg(AMBER_DEEP);
pub const SEGMENT_PARTIAL: Style = fg(AMBER);
/// Where a connection is writing.
pub const SEGMENT_CURSOR: Style = fg(AMBER_GLOW);
pub const SEGMENT_MISSING: Style = fg(HOLLOW);
pub const GRAPH: Style = fg(EMBER);
/// A key in the footer, before what it does.
pub const KEY: Style = bold(STONE);
pub const TABLE_HEADER: Style = bold(palette::MUTED);
pub const SELECTED_ROW: Style = Style::new()
    .fg(BRIGHT)
    .bg(SELECTED_BG)
    .add_modifier(Modifier::BOLD);
pub const BUTTON: Style = Style::new().fg(STONE).bg(BUTTON_BG);
pub const BUTTON_ADD: Style = Style::new().fg(ON_AMBER).bg(AMBER);
pub const BUTTON_QUIT: Style = Style::new().fg(palette::MUTED).bg(QUIT_BG);
/// A text field that does not have the cursor.
pub const FIELD: Style = Style::new().fg(STONE).bg(FIELD_BG);
/// The text field being typed in.
pub const FIELD_FOCUSED: Style = Style::new().fg(BRIGHT).bg(BUTTON_BG);
/// Laid over the screen behind an open dialog.
pub const BEHIND_DIALOG: Style = Style::new().add_modifier(Modifier::DIM);
/// A prompt waiting for input.
pub const PROMPT: Style = bold(SAGE);
/// A question that needs an answer before anything else happens.
pub const CONFIRM: Style = bold(AMBER);
/// An active download that has had no data for a while.
pub const STALLED: Style = fg(CORAL);

pub fn status_style(status: DownloadStatus) -> Style {
    match status {
        DownloadStatus::Active => fg(AMBER),
        DownloadStatus::Queued => fg(STONE),
        DownloadStatus::Pausing | DownloadStatus::Paused => fg(FAINT),
        DownloadStatus::Retrying => fg(LAVENDER),
        DownloadStatus::Completed => fg(SAGE),
        DownloadStatus::Failed => fg(RED),
        DownloadStatus::Cancelled => fg(DUSK),
        // Waiting for the user, so it stands out from the downloads that run alone.
        DownloadStatus::Choosing => CONFIRM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choosing_is_amber_and_bold_so_it_reads_as_needing_you() {
        let style = status_style(DownloadStatus::Choosing);
        assert_eq!(style.fg, Some(AMBER));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(style, status_style(DownloadStatus::Active));
    }

    #[test]
    fn every_status_has_its_own_colour_except_the_two_kinds_of_pause() {
        let all = [
            DownloadStatus::Queued,
            DownloadStatus::Active,
            DownloadStatus::Paused,
            DownloadStatus::Retrying,
            DownloadStatus::Completed,
            DownloadStatus::Failed,
            DownloadStatus::Cancelled,
            DownloadStatus::Choosing,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(status_style(*a), status_style(*b), "{a:?} and {b:?}");
            }
        }
        assert_eq!(
            status_style(DownloadStatus::Pausing),
            status_style(DownloadStatus::Paused)
        );
        assert_ne!(STALLED, status_style(DownloadStatus::Active));
    }
}
