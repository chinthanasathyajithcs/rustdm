//! The key table for each screen, shown by the Help overlay and the footer.

use crate::tui::state::Screen;
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Widest the Help box gets.
const HELP_WIDTH: u16 = 52;
/// Column the descriptions start in.
const KEY_COLUMN: usize = 14;

const DOWNLOADS_KEYS: &[(&str, &str)] = &[
    ("a", "Add"),
    ("p", "Pause"),
    ("r", "Resume"),
    ("c", "Cancel"),
    ("x", "Remove"),
    ("v", "Speed cap"),
    ("o", "Open folder"),
    ("s", "Settings"),
    ("l", "Logs"),
    ("?", "Help"),
    ("q", "Quit"),
    ("Q", "Stop rdmd"),
    ("←/→", "Tabs"),
    ("↑/↓", "Select"),
    ("⇧↑/⇧↓", "Reorder"),
    ("1–5", "Tab by number"),
    ("PgUp/PgDn", "Page up or down"),
    ("Home/End", "First or last"),
    ("Ctrl+C", "Quit"),
];

const SETTINGS_KEYS: &[(&str, &str)] = &[
    ("↑/↓", "Select"),
    ("Enter", "Edit or save"),
    ("Esc", "Back"),
    ("l", "Logs"),
    ("?", "Help"),
    ("q", "Quit"),
    ("Q", "Stop rdmd"),
];

const LOGS_KEYS: &[(&str, &str)] = &[
    ("↑/↓", "Scroll"),
    ("PgUp/PgDn", "Scroll a page"),
    ("G", "Newest line"),
    ("Esc", "Back"),
    ("s", "Settings"),
    ("?", "Help"),
    ("q", "Quit"),
    ("Q", "Stop rdmd"),
];

/// Every key for `screen`, most useful first.
pub fn keys(screen: Screen) -> &'static [(&'static str, &'static str)] {
    match screen {
        Screen::Downloads => DOWNLOADS_KEYS,
        Screen::Settings => SETTINGS_KEYS,
        Screen::Logs => LOGS_KEYS,
    }
}

/// Draw the Help box centred over `area`, clipped to it.
pub fn render_overlay(frame: &mut Frame, screen: Screen, area: Rect) {
    let keys = keys(screen);
    let width = HELP_WIDTH.min(area.width);
    let height = (keys.len() as u16 + 3).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(key, does)| {
            Line::from(vec![
                Span::styled(format!("{key:<KEY_COLUMN$}"), theme::ACCENT),
                Span::raw(*does),
            ])
        })
        .collect();
    lines.push(Line::from(Span::styled("? or Esc closes Help", theme::DIM)));
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" Help: {} ", screen.title()))
        .border_style(theme::ACCENT);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}
