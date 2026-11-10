//! Dialogs: boxes drawn over the current screen. Only the top one of the
//! stack gets keys.

pub mod add;

use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::theme;
use add::AddDialog;
use ratatui::crossterm::event::{Event, KeyCode};
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;

/// What a dialog did with an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogResult {
    /// The dialog handled it and stays open.
    Used,
    /// Close the dialog without doing anything.
    Close,
    /// Carry out this action; the dialog waits for the result.
    Submit(Action),
}

/// A clickable part of a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogTarget {
    /// The field at this position, top to bottom.
    Field(usize),
    /// A button, which acts as its key.
    Key(KeyCode),
}

#[derive(Clone, Debug)]
pub enum Dialog {
    Add(AddDialog),
}

impl Dialog {
    pub fn handle_event(&mut self, event: &Event) -> DialogResult {
        match self {
            Dialog::Add(d) => d.handle_event(event),
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match self {
            Dialog::Add(d) => d.click(target),
        }
    }

    /// Draw the dialog over `area` and return its click targets.
    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        match self {
            Dialog::Add(d) => d.render(frame, area),
        }
    }

    /// Show why the submitted action failed, and wait for the user again.
    pub fn set_error(&mut self, err: &ActionError) {
        match self {
            Dialog::Add(d) => d.set_error(err),
        }
    }

    /// True between a submit and its result.
    pub fn is_pending(&self) -> bool {
        match self {
            Dialog::Add(d) => d.is_pending(),
        }
    }
}

/// A box of the wanted size centred on `screen`, clamped to fit it.
pub(crate) fn centered(screen: Rect, wanted_width: u16, wanted_height: u16) -> Rect {
    let width = wanted_width.min(screen.width);
    let height = wanted_height.min(screen.height);
    Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    )
}

/// Clear `area` and draw a bordered box titled `title`; returns the inside.
pub(crate) fn draw_box(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(theme::ACCENT);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}
