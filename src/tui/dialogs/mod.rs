//! Dialogs: boxes drawn over the current screen. Only the top one of the
//! stack gets keys.

pub mod add;
pub mod confirm;
pub mod quality;
pub mod speed;

use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::theme;
use add::AddDialog;
use confirm::ConfirmDialog;
use quality::QualityDialog;
use ratatui::crossterm::event::{Event, KeyCode};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use speed::SpeedDialog;

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
    /// The row at this position in a list of choices, top to bottom.
    Row(usize),
    /// A button, which acts as its key.
    Key(KeyCode),
}

#[derive(Clone, Debug)]
pub enum Dialog {
    // Boxed: the Add dialog is several times the size of the others.
    Add(Box<AddDialog>),
    // Boxed: it holds the whole list of qualities.
    Quality(Box<QualityDialog>),
    Confirm(ConfirmDialog),
    Speed(SpeedDialog),
}

impl Dialog {
    pub fn handle_event(&mut self, event: &Event) -> DialogResult {
        match self {
            Dialog::Add(d) => d.handle_event(event),
            Dialog::Quality(d) => d.handle_event(event),
            Dialog::Confirm(d) => d.handle_event(event),
            Dialog::Speed(d) => d.handle_event(event),
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match self {
            Dialog::Add(d) => d.click(target),
            Dialog::Quality(d) => d.click(target),
            Dialog::Confirm(d) => d.click(target),
            Dialog::Speed(d) => d.click(target),
        }
    }

    /// Draw the dialog over `area` and return its click targets.
    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        match self {
            Dialog::Add(d) => d.render(frame, area),
            Dialog::Quality(d) => d.render(frame, area),
            Dialog::Confirm(d) => d.render(frame, area),
            Dialog::Speed(d) => d.render(frame, area),
        }
    }

    /// Show why the submitted action failed, and wait for the user again.
    pub fn set_error(&mut self, err: &ActionError) {
        match self {
            Dialog::Add(d) => d.set_error(err),
            Dialog::Quality(d) => d.set_error(err),
            Dialog::Confirm(d) => d.set_error(err),
            Dialog::Speed(d) => d.set_error(err),
        }
    }

    /// True between a submit and its result.
    pub fn is_pending(&self) -> bool {
        match self {
            Dialog::Add(d) => d.is_pending(),
            Dialog::Quality(d) => d.is_pending(),
            Dialog::Confirm(d) => d.is_pending(),
            Dialog::Speed(d) => d.is_pending(),
        }
    }
}

/// A box of the wanted size centred on `screen`, clamped to fit it.
pub fn dialog_area(screen: Rect, wanted_width: u16, wanted_height: u16) -> Rect {
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

/// Draw `line` on `row` of `inner`, if that row exists.
pub(crate) fn put(frame: &mut Frame, inner: Rect, row: u16, line: Line) {
    if row < inner.height {
        let rect = Rect::new(inner.x, inner.y + row, inner.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
    }
}

/// Buttons side by side on `row` of `inner` from column `x`, two cells apart.
/// Each acts as its key when clicked; those that don't fit are left out.
pub(crate) fn render_buttons(
    frame: &mut Frame,
    inner: Rect,
    row: u16,
    x: u16,
    buttons: &[(&str, KeyCode, Style)],
) -> Vec<(Rect, DialogTarget)> {
    let mut hits = Vec::new();
    if row >= inner.height {
        return hits;
    }
    let mut x = x;
    for (label, code, style) in buttons {
        let width = label.chars().count() as u16;
        if x + width > inner.right() {
            break;
        }
        let rect = Rect::new(x, inner.y + row, width, 1);
        frame.render_widget(Paragraph::new(*label).style(*style), rect);
        hits.push((rect, DialogTarget::Key(*code)));
        x += width + 2;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::add::AddDialog;
    use super::confirm::ConfirmDialog;
    use super::speed::SpeedDialog;
    use super::*;
    use crate::tui::state::test_support::{choosing_summary, sample_app};
    use crate::tui::state::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn dialog_area_is_centred_and_clamped_to_the_screen() {
        let screen = Rect::new(0, 0, 100, 30);
        assert_eq!(dialog_area(screen, 60, 10), Rect::new(20, 10, 60, 10));
        let tiny = Rect::new(0, 0, 30, 8);
        assert_eq!(dialog_area(tiny, 60, 12), tiny);
        assert_eq!(dialog_area(Rect::new(0, 0, 0, 0), 60, 12).area(), 0);
    }

    #[test]
    fn dialogs_fit_tiny_terminals_without_panicking() {
        let base = sample_app();
        let d = base.downloads[0].clone();
        let dialogs = [
            Dialog::Add(Box::new(AddDialog::new(
                None,
                Some("https://example.com/a"),
            ))),
            Dialog::Confirm(ConfirmDialog::cancel(&d)),
            Dialog::Confirm(ConfirmDialog::stop_daemon(2)),
            Dialog::Speed(SpeedDialog::new(&d)),
            Dialog::Quality(Box::new(
                quality::QualityDialog::new(&choosing_summary("dl_9"), None).unwrap(),
            )),
        ];
        for (width, height) in [(30, 8), (10, 3), (1, 1)] {
            for dialog in &dialogs {
                let mut app = sample_app();
                app.dialogs.push(dialog.clone());
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|f| crate::tui::screens::render(f, &mut app))
                    .unwrap();
                for (rect, _) in &app.hits.dialog {
                    assert!(
                        rect.right() <= width && rect.bottom() <= height,
                        "{rect:?} outside {width}x{height}"
                    );
                }
            }
            let mut app = App {
                help_open: true,
                ..sample_app()
            };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| crate::tui::screens::render(f, &mut app))
                .unwrap();
        }
    }
}
