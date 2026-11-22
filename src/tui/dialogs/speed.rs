//! The speed cap of one download.

use super::{dialog_area, draw_box, put, render_buttons, DialogResult, DialogTarget};
use crate::ipc::protocol::DownloadSummary;
use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::format::format_speed_cap;
use crate::tui::theme;
use crate::tui::widgets::text_field::TextField;
use crate::units::parse_speed;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const SPEED_WIDTH: u16 = 56;
/// Borders, the field, the hint or error, and the buttons.
const SPEED_HEIGHT: u16 = 5;
const LABEL_WIDTH: u16 = 11;
const HINT: &str = "500K, 2M, or off";

#[derive(Clone, Debug)]
pub struct SpeedDialog {
    id: String,
    title: String,
    field: TextField,
    error: Option<String>,
    pending: bool,
}

impl SpeedDialog {
    /// The speed cap of `summary`, starting from its current one.
    pub fn new(summary: &DownloadSummary) -> Self {
        Self {
            id: summary.id.clone(),
            title: format!("Speed cap for {}", summary.filename),
            field: TextField::new(&format_speed_cap(summary.max_bytes_per_sec)),
            error: None,
            pending: false,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn text(&self) -> &str {
        self.field.text()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn handle_event(&mut self, event: &Event) -> DialogResult {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => DialogResult::Close,
                _ if self.pending => DialogResult::Used,
                KeyCode::Enter => self.submit(),
                _ => {
                    self.field.handle_key(key);
                    DialogResult::Used
                }
            },
            Event::Paste(text) if !self.pending => {
                self.field.paste(text);
                DialogResult::Used
            }
            _ => DialogResult::Used,
        }
    }

    fn submit(&mut self) -> DialogResult {
        match parse_speed(self.field.text()) {
            Ok(max_bytes_per_sec) => {
                self.error = None;
                self.pending = true;
                DialogResult::Submit(Action::SetSpeed {
                    id: self.id.clone(),
                    max_bytes_per_sec,
                })
            }
            Err(e) => {
                self.error = Some(e);
                DialogResult::Used
            }
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match target {
            DialogTarget::Key(code) => {
                self.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            }
            DialogTarget::Field(_) | DialogTarget::Row(_) => DialogResult::Used,
        }
    }

    pub fn set_error(&mut self, err: &ActionError) {
        self.pending = false;
        self.error = Some(err.message.clone());
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        let inner = draw_box(
            frame,
            dialog_area(area, SPEED_WIDTH, SPEED_HEIGHT),
            &self.title,
        );
        let mut hits = Vec::new();
        if inner.height > 0 && inner.width > LABEL_WIDTH {
            let label = Rect::new(inner.x, inner.y, LABEL_WIDTH, 1);
            frame.render_widget(Paragraph::new("Speed cap").style(theme::MUTED), label);
            let value = Rect::new(inner.x + LABEL_WIDTH, inner.y, inner.width - LABEL_WIDTH, 1);
            self.field.render(frame, value, !self.pending);
            hits.push((value, DialogTarget::Field(0)));
        }
        let under = match &self.error {
            Some(err) => Span::styled(err.clone(), theme::ERROR),
            None => Span::styled(HINT, theme::DIM),
        };
        let indent = Span::raw(" ".repeat(LABEL_WIDTH as usize));
        put(frame, inner, 1, Line::from(vec![indent, under]));
        hits.extend(render_buttons(
            frame,
            inner,
            2,
            inner.x + LABEL_WIDTH,
            &[
                ("[ Enter Save ]", KeyCode::Enter, theme::BUTTON_ADD),
                ("[ Esc Cancel ]", KeyCode::Esc, theme::BUTTON),
            ],
        ));
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::dialogs::Dialog;
    use crate::tui::screens::handle_event;
    use crate::tui::state::test_support::app_with;
    use crate::tui::state::App;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            handle_event(app, &key(KeyCode::Char(c)));
        }
    }

    fn speed_dialog(app: &App) -> &SpeedDialog {
        match app.dialogs.last() {
            Some(Dialog::Speed(d)) => d,
            other => panic!("expected the speed dialog, got {other:?}"),
        }
    }

    fn capped_app() -> App {
        let mut app = app_with(&[DownloadStatus::Active]);
        app.downloads[0].max_bytes_per_sec = 2 * 1024 * 1024;
        app
    }

    #[test]
    fn v_opens_the_speed_dialog_prefilled_with_the_current_cap() {
        let mut app = capped_app();
        handle_event(&mut app, &key(KeyCode::Char('v')));
        let dialog = speed_dialog(&app);
        assert_eq!(dialog.title(), "Speed cap for dl_1.bin");
        assert_eq!(dialog.text(), "2.0 MiB/s");

        // Enter on the unchanged value keeps the same cap.
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Enter)),
            Some(Action::SetSpeed {
                id: "dl_1".into(),
                max_bytes_per_sec: 2 * 1024 * 1024
            })
        );
    }

    #[test]
    fn off_removes_the_speed_cap() {
        let mut app = capped_app();
        handle_event(&mut app, &key(KeyCode::Char('v')));
        handle_event(
            &mut app,
            &Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        );
        type_text(&mut app, "off");
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Enter)),
            Some(Action::SetSpeed {
                id: "dl_1".into(),
                max_bytes_per_sec: 0
            })
        );
    }

    #[test]
    fn a_bad_speed_shows_the_parse_error_under_the_field() {
        let mut app = capped_app();
        handle_event(&mut app, &key(KeyCode::Char('v')));
        type_text(&mut app, "x");
        assert_eq!(handle_event(&mut app, &key(KeyCode::Enter)), None);
        let dialog = speed_dialog(&app);
        assert_eq!(
            dialog.error(),
            Some("unknown speed unit 'mib/sx'; use K, M, G or T")
        );
        assert!(!dialog.is_pending());
    }

    #[test]
    fn without_a_selection_v_explains() {
        let mut app = App::default();
        handle_event(&mut app, &key(KeyCode::Char('v')));
        assert!(app.dialogs.is_empty());
        assert_eq!(app.notice.as_ref().unwrap().text, "Select a download first");
    }
}
