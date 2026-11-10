//! The Settings screen.

use crate::tui::state::App;
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn render(frame: &mut Frame, _app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).title(" Settings ");
    let text = Paragraph::new("Not built yet").style(theme::MUTED);
    frame.render_widget(text.block(block), area);
}
