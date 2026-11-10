//! Questions with a few one-key answers: cancelling a download, stopping rdmd.

use super::{dialog_area, draw_box, put, render_buttons, DialogResult, DialogTarget};
use crate::ipc::protocol::DownloadSummary;
use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::theme;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

const CONFIRM_WIDTH: u16 = 84;

/// One answer: the key that gives it, what it says, and what it does.
/// No action means "leave things as they are".
#[derive(Clone, Debug)]
struct Choice {
    key: char,
    label: &'static str,
    action: Option<Action>,
}

#[derive(Clone, Debug)]
pub struct ConfirmDialog {
    title: &'static str,
    question: String,
    choices: Vec<Choice>,
    error: Option<String>,
    pending: bool,
}

impl ConfirmDialog {
    /// "Cancel dl_3 (ubuntu.iso)?", keeping or deleting the partial files.
    pub fn cancel(summary: &DownloadSummary) -> Self {
        let cancel = |delete_files| {
            Some(Action::Cancel {
                id: summary.id.clone(),
                delete_files,
            })
        };
        Self::new(
            "Cancel download",
            format!("Cancel {} ({})?", summary.id, summary.filename),
            vec![
                choice('y', "Cancel", cancel(false)),
                choice('d', "Cancel and delete partial files", cancel(true)),
                choice('n', "Keep it", None),
            ],
        )
    }

    /// Stopping rdmd pauses the `active` downloads until it starts again.
    pub fn stop_daemon(active: usize) -> Self {
        let downloads = match active {
            1 => "1 download will".to_string(),
            n => format!("{n} downloads will"),
        };
        Self::new(
            "Stop rdmd",
            format!("Stop rdmd? {downloads} pause and continue the next time it starts."),
            vec![
                choice('y', "Stop", Some(Action::StopDaemon)),
                choice('n', "Keep running", None),
            ],
        )
    }

    fn new(title: &'static str, question: String, choices: Vec<Choice>) -> Self {
        Self {
            title,
            question,
            choices,
            error: None,
            pending: false,
        }
    }

    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn handle_event(&mut self, event: &Event) -> DialogResult {
        let Event::Key(key) = event else {
            return DialogResult::Used;
        };
        if key.kind != KeyEventKind::Press {
            return DialogResult::Used;
        }
        if key.code == KeyCode::Esc {
            return DialogResult::Close;
        }
        if self.pending {
            return DialogResult::Used;
        }
        let picked = self
            .choices
            .iter()
            .find(|c| key.code == KeyCode::Char(c.key))
            .map(|c| c.action.clone());
        match picked {
            Some(Some(action)) => {
                self.pending = true;
                DialogResult::Submit(action)
            }
            Some(None) => DialogResult::Close,
            None => DialogResult::Used,
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match target {
            DialogTarget::Key(code) => {
                self.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            }
            DialogTarget::Field(_) => DialogResult::Used,
        }
    }

    pub fn set_error(&mut self, err: &ActionError) {
        self.pending = false;
        self.error = Some(err.message.clone());
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        let text_width = CONFIRM_WIDTH.min(area.width).saturating_sub(2).max(1);
        let question_rows = (self.question.chars().count() as u16)
            .div_ceil(text_width)
            .max(1);
        let height = question_rows + 4;
        let inner = draw_box(frame, dialog_area(area, CONFIRM_WIDTH, height), self.title);
        let question = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            question_rows.min(inner.height),
        );
        frame.render_widget(
            Paragraph::new(self.question.as_str()).wrap(Wrap { trim: true }),
            question,
        );
        let status = match (&self.error, self.pending) {
            (Some(err), _) => Span::styled(err.clone(), theme::ERROR),
            (None, true) => Span::styled("Working…", theme::MUTED),
            (None, false) => Span::raw(""),
        };
        put(frame, inner, question_rows, Line::from(status));
        let labels: Vec<String> = self
            .choices
            .iter()
            .map(|c| format!("{} {}", c.key, c.label))
            .collect();
        let buttons: Vec<_> = self
            .choices
            .iter()
            .zip(&labels)
            .map(|(c, label)| (label.as_str(), KeyCode::Char(c.key), theme::ACCENT))
            .collect();
        render_buttons(frame, inner, question_rows + 1, inner.x, &buttons)
    }
}

fn choice(key: char, label: &'static str, action: Option<Action>) -> Choice {
    Choice { key, label, action }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::dialogs::{Dialog, DialogTarget};
    use crate::tui::screens::{handle_event, render};
    use crate::tui::state::test_support::{app_with, screen_text};
    use crate::tui::state::App;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn asking_to_cancel() -> App {
        let mut app = app_with(&[DownloadStatus::Active]);
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('c'))), None);
        assert!(matches!(app.dialogs.last(), Some(Dialog::Confirm(_))));
        app
    }

    fn draw(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        screen_text(&terminal)
    }

    #[test]
    fn c_asks_and_y_cancels_keeping_files() {
        let mut app = asking_to_cancel();
        let text = draw(&mut app);
        assert!(text.contains("Cancel dl_1 (dl_1.bin)?"), "{text}");
        assert!(
            text.contains("y Cancel  d Cancel and delete partial files  n Keep it"),
            "{text}"
        );
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('y'))),
            Some(Action::Cancel {
                id: "dl_1".into(),
                delete_files: false
            })
        );
    }

    #[test]
    fn d_cancels_and_deletes_partial_files() {
        let mut app = asking_to_cancel();
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('d'))),
            Some(Action::Cancel {
                id: "dl_1".into(),
                delete_files: true
            })
        );
    }

    #[test]
    fn n_or_esc_keeps_the_download() {
        for answer in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = asking_to_cancel();
            assert_eq!(handle_event(&mut app, &key(KeyCode::Char('x'))), None);
            assert_eq!(app.dialogs.len(), 1, "other keys do nothing");
            assert_eq!(handle_event(&mut app, &key(answer)), None);
            assert!(app.dialogs.is_empty(), "{answer:?} closes it");
        }
    }

    #[test]
    fn clicking_a_choice_picks_it() {
        let mut app = asking_to_cancel();
        draw(&mut app);
        let (rect, _) = *app
            .hits
            .dialog
            .iter()
            .find(|(_, t)| *t == DialogTarget::Key(KeyCode::Char('d')))
            .expect("the d choice is clickable");
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            handle_event(&mut app, &click),
            Some(Action::Cancel {
                id: "dl_1".into(),
                delete_files: true
            })
        );
    }

    #[test]
    fn capital_q_asks_before_stopping_and_counts_active_downloads() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Active, Queued]);
        app.connected = true;
        let shift_q = Event::Key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT));
        assert_eq!(handle_event(&mut app, &shift_q), None);
        let text = draw(&mut app);
        assert!(
            text.contains(
                "Stop rdmd? 2 downloads will pause and continue the next time it starts."
            ),
            "{text}"
        );
        assert!(text.contains("y Stop  n Keep running"), "{text}");
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('y'))),
            Some(Action::StopDaemon)
        );
    }

    #[test]
    fn one_active_download_is_counted_in_the_singular() {
        let dialog = ConfirmDialog::stop_daemon(1);
        assert_eq!(
            dialog.question(),
            "Stop rdmd? 1 download will pause and continue the next time it starts."
        );
    }
}
