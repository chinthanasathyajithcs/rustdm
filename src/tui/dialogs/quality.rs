//! The quality list: the qualities a video page offers, with the folder and
//! file name the video will be saved under.

use super::{dialog_area, draw_box, put, render_buttons, DialogResult, DialogTarget};
use crate::dirs::{full_folder, home_dir};
use crate::ipc::protocol::{
    ChooseQualityParams, DownloadDefaults, DownloadSummary, CODE_DESTINATION_EXISTS,
    CODE_INVALID_PATH, CODE_PERMISSION_DENIED,
};
use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::format::format_bytes;
use crate::tui::theme;
use crate::tui::widgets::text_field::TextField;
use crate::video::VideoChoice;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::path::Path;

const NO_NAME: &str = "Enter a file name";
const DIALOG_WIDTH: u16 = 76;
const LABEL_WIDTH: u16 = 8;
/// Rows that are not quality rows: the title, Folder, Name, the message and the buttons.
const FIXED_ROWS: u16 = 5;
/// A row this tall or taller is marked "(4K)".
const FOUR_K_HEIGHT: u32 = 2160;
/// "WebM", the longest container name.
const CONTAINER_WIDTH: usize = 4;

/// Which part of the dialog takes typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityFocus {
    List,
    Folder,
    Name,
}

impl QualityFocus {
    const ALL: [QualityFocus; 3] = [QualityFocus::List, QualityFocus::Folder, QualityFocus::Name];

    fn shifted(self, step: usize) -> Self {
        let at = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(at + step) % Self::ALL.len()]
    }
}

#[derive(Clone, Debug)]
pub struct QualityDialog {
    id: String,
    title: String,
    choices: Vec<VideoChoice>,
    cursor: usize,
    focus: QualityFocus,
    folder: TextField,
    name: TextField,
    /// What the fields held when the dialog opened; only a change is sent.
    start_folder: String,
    start_name: String,
    error: Option<String>,
    pending: bool,
}

impl QualityDialog {
    /// The list for a download that is waiting for a quality, or `None` when
    /// it has no rows to offer. The folder and name start from the download's
    /// own; `defaults` stand in for a folder it doesn't have.
    pub fn new(summary: &DownloadSummary, defaults: Option<&DownloadDefaults>) -> Option<Self> {
        let video = summary.video.as_ref().filter(|v| !v.choices.is_empty())?;
        let cursor = video
            .default
            .filter(|i| *i < video.choices.len())
            .unwrap_or(0);
        let folder = if summary.destination.as_os_str().is_empty() {
            defaults.map(|d| d.folder.display().to_string())
        } else {
            Some(summary.destination.display().to_string())
        }
        .unwrap_or_default();
        let name = Path::new(&summary.filename)
            .file_stem()
            .map_or_else(|| summary.filename.clone(), |s| s.to_string_lossy().into());
        Some(Self {
            id: summary.id.clone(),
            title: video.title.clone(),
            choices: video.choices.clone(),
            cursor,
            focus: QualityFocus::List,
            folder: TextField::new(&folder),
            name: TextField::new(&name),
            start_folder: folder,
            start_name: name,
            error: None,
            pending: false,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn focused(&self) -> QualityFocus {
        self.focus
    }

    pub fn highlighted(&self) -> &VideoChoice {
        &self.choices[self.cursor]
    }

    pub fn folder_text(&self) -> &str {
        self.folder.text()
    }

    pub fn name_text(&self) -> &str {
        self.name.text()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn handle_event(&mut self, event: &Event) -> DialogResult {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if self.pending {
                    // Waiting for the daemon; only closing is allowed.
                    return if key.code == KeyCode::Esc {
                        DialogResult::Close
                    } else {
                        DialogResult::Used
                    };
                }
                self.handle_key(key)
            }
            Event::Paste(text) if !self.pending => {
                match self.focus {
                    QualityFocus::List => {}
                    QualityFocus::Folder => self.folder.paste(text),
                    QualityFocus::Name => self.name.paste(text),
                }
                DialogResult::Used
            }
            _ => DialogResult::Used,
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match target {
            DialogTarget::Key(code) => {
                self.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            }
            _ if self.pending => DialogResult::Used,
            DialogTarget::Row(i) if i < self.choices.len() => {
                self.cursor = i;
                self.focus = QualityFocus::List;
                DialogResult::Used
            }
            DialogTarget::Field(0) => {
                self.focus = QualityFocus::Folder;
                DialogResult::Used
            }
            DialogTarget::Field(1) => {
                self.focus = QualityFocus::Name;
                DialogResult::Used
            }
            _ => DialogResult::Used,
        }
    }

    fn handle_key(&mut self, key: &KeyEvent) -> DialogResult {
        let last = self.choices.len() - 1;
        match key.code {
            KeyCode::Esc => return DialogResult::Close,
            KeyCode::Enter => return self.submit(),
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Tab => self.focus = self.focus.shifted(1),
            KeyCode::BackTab => self.focus = self.focus.shifted(QualityFocus::ALL.len() - 1),
            KeyCode::Home if self.focus == QualityFocus::List => self.cursor = 0,
            KeyCode::End if self.focus == QualityFocus::List => self.cursor = last,
            _ => {
                match self.focus {
                    QualityFocus::List => false,
                    QualityFocus::Folder => self.folder.handle_key(key),
                    QualityFocus::Name => self.name.handle_key(key),
                };
            }
        }
        DialogResult::Used
    }

    fn submit(&mut self) -> DialogResult {
        self.error = None;
        match self.params() {
            Ok(params) => {
                self.pending = true;
                DialogResult::Submit(Action::ChooseQuality(params))
            }
            Err((focus, message)) => {
                self.error = Some(message);
                self.focus = focus;
                DialogResult::Used
            }
        }
    }

    /// Check the fields like the Add dialog does; the folder and name are
    /// included only when the user changed them.
    fn params(&self) -> Result<ChooseQualityParams, (QualityFocus, String)> {
        let folder = self.folder.text().trim();
        let destination = if folder == self.start_folder {
            None
        } else {
            let home = home_dir();
            let path =
                full_folder(folder, home.as_deref()).map_err(|e| (QualityFocus::Folder, e))?;
            Some(path)
        };
        let name = self.name.text().trim();
        if name.is_empty() {
            return Err((QualityFocus::Name, NO_NAME.to_string()));
        }
        Ok(ChooseQualityParams {
            id: self.id.clone(),
            choice_id: self.highlighted().id.clone(),
            destination,
            filename: (name != self.start_name).then(|| name.to_string()),
        })
    }

    /// Show why the choice failed, and wait for the user again.
    pub fn set_error(&mut self, err: &ActionError) {
        self.pending = false;
        self.error = Some(err.message.clone());
        let folder_problem = matches!(
            err.code,
            Some(CODE_INVALID_PATH | CODE_PERMISSION_DENIED | CODE_DESTINATION_EXISTS)
        );
        if folder_problem {
            self.focus = QualityFocus::Folder;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        let wanted_height = 2 + FIXED_ROWS + self.choices.len() as u16;
        let inner = draw_box(
            frame,
            dialog_area(area, DIALOG_WIDTH, wanted_height),
            "Choose a quality",
        );
        let mut hits = Vec::new();
        put(
            frame,
            inner,
            0,
            Line::from(Span::styled(self.title.clone(), theme::PANEL_TITLE)),
        );
        let shown = self.render_rows(frame, inner, &mut hits);
        let folder_row = 1 + shown;
        let fields = [
            ("Folder", &self.folder, QualityFocus::Folder),
            ("Name", &self.name, QualityFocus::Name),
        ];
        for (i, (label, field, focus)) in fields.into_iter().enumerate() {
            let row = folder_row + i as u16;
            if let Some(rect) = self.render_field(frame, inner, row, label, field, focus) {
                hits.push((rect, DialogTarget::Field(i)));
            }
        }
        let message = match (&self.error, self.pending) {
            (Some(err), _) => Span::styled(err.clone(), theme::ERROR),
            (None, true) => Span::styled("Starting…", theme::MUTED),
            (None, false) => {
                Span::styled("Up and Down choose · Tab moves to the fields", theme::DIM)
            }
        };
        put(frame, inner, folder_row + 2, Line::from(message));
        hits.extend(render_buttons(
            frame,
            inner,
            folder_row + 3,
            inner.x,
            &[
                ("[ Enter Download ]", KeyCode::Enter, theme::BUTTON_ADD),
                ("[ Esc Close ]", KeyCode::Esc, theme::BUTTON),
            ],
        ));
        hits
    }

    /// Draw as many rows as fit, scrolled so the highlighted one shows.
    /// Returns how many rows were drawn.
    fn render_rows(
        &self,
        frame: &mut Frame,
        inner: Rect,
        hits: &mut Vec<(Rect, DialogTarget)>,
    ) -> u16 {
        let room = inner.height.saturating_sub(FIXED_ROWS) as usize;
        let start = (self.cursor + 1).saturating_sub(room);
        let label_width = self.choices.iter().map(|c| c.label.chars().count()).max();
        let label_width = label_width.unwrap_or(0);
        let shown = self.choices.iter().enumerate().skip(start).take(room);
        let mut count = 0;
        for (i, choice) in shown {
            let row = 1 + count;
            let on_cursor = i == self.cursor;
            let marker = if on_cursor { "> " } else { "  " };
            let style = if on_cursor {
                theme::SELECTED_ROW
            } else {
                theme::MUTED
            };
            let text = format!("{marker}{}", row_text(choice, label_width));
            let line = Line::from(Span::styled(text, style));
            let rect = Rect::new(inner.x, inner.y + row, inner.width, 1);
            frame.render_widget(Paragraph::new(line).style(style), rect);
            hits.push((rect, DialogTarget::Row(i)));
            count += 1;
        }
        count
    }

    /// Draw one field's label and value; returns the value's area if it fits.
    fn render_field(
        &self,
        frame: &mut Frame,
        inner: Rect,
        row: u16,
        label: &str,
        field: &TextField,
        focus: QualityFocus,
    ) -> Option<Rect> {
        if row >= inner.height || inner.width <= LABEL_WIDTH {
            return None;
        }
        let y = inner.y + row;
        let label_rect = Rect::new(inner.x, y, LABEL_WIDTH, 1);
        frame.render_widget(Paragraph::new(label).style(theme::MUTED), label_rect);
        let value = Rect::new(inner.x + LABEL_WIDTH, y, inner.width - LABEL_WIDTH, 1);
        let focused = focus == self.focus && !self.pending;
        let style = if focused {
            theme::FIELD_FOCUSED
        } else {
            theme::FIELD
        };
        frame.render_widget(Paragraph::new("").style(style), value);
        field.render(frame, value, focused);
        Some(value)
    }
}

/// "1080p  MP4   8.6 MiB": the label padded to `label_width`, the container,
/// then the size, with `~` in front of an estimate and "(4K)" after a 2160p
/// row. An unknown size is left blank.
pub fn row_text(choice: &VideoChoice, label_width: usize) -> String {
    let size = choice.size_bytes.map_or(String::new(), |bytes| {
        let estimate = if choice.size_is_estimate { "~" } else { "" };
        format!("{estimate}{}", format_bytes(bytes))
    });
    let four_k = if choice.height.is_some_and(|h| h >= FOUR_K_HEIGHT) {
        " (4K)"
    } else {
        ""
    };
    let container = choice.container.label();
    format!(
        "{:<label_width$}  {container:<CONTAINER_WIDTH$}  {size}{four_k}",
        choice.label
    )
    .trim_end()
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{ChooseQualityParams, CODE_INVALID_PATH};
    use crate::tui::dialogs::Dialog;
    use crate::tui::state::test_support::{choosing_summary, sample_app, screen_text};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press(dialog: &mut QualityDialog, code: KeyCode) -> DialogResult {
        dialog.handle_event(&key(code))
    }

    fn dialog() -> QualityDialog {
        QualityDialog::new(&choosing_summary("dl_1"), None).expect("a quality list")
    }

    fn chosen(result: DialogResult) -> ChooseQualityParams {
        match result {
            DialogResult::Submit(Action::ChooseQuality(params)) => params,
            other => panic!("expected a ChooseQuality submit, got {other:?}"),
        }
    }

    #[test]
    fn there_is_no_list_without_choices() {
        let mut summary = choosing_summary("dl_1");
        summary.video.as_mut().unwrap().choices.clear();
        assert!(QualityDialog::new(&summary, None).is_none());
        summary.video = None;
        assert!(QualityDialog::new(&summary, None).is_none());
    }

    #[test]
    fn the_list_opens_on_the_default_row() {
        assert_eq!(dialog().highlighted().label, "1080p");
        let mut summary = choosing_summary("dl_1");
        summary.video.as_mut().unwrap().default = None;
        let first = QualityDialog::new(&summary, None).unwrap();
        assert_eq!(first.highlighted().label, "2160p");
    }

    #[test]
    fn enter_takes_the_highlighted_row() {
        let mut dialog = dialog();
        let params = chosen(press(&mut dialog, KeyCode::Enter));
        assert_eq!(
            params,
            ChooseQualityParams {
                id: "dl_1".into(),
                choice_id: "1080-mp4".into(),
                destination: None,
                filename: None,
            }
        );
        assert!(dialog.is_pending());
    }

    #[test]
    fn arrows_move_and_stop_at_the_ends() {
        let mut dialog = dialog();
        press(&mut dialog, KeyCode::Down);
        assert_eq!(dialog.highlighted().label, "720p");
        for _ in 0..10 {
            press(&mut dialog, KeyCode::Down);
        }
        assert_eq!(dialog.highlighted().label, "Audio only");
        for _ in 0..10 {
            press(&mut dialog, KeyCode::Up);
        }
        assert_eq!(dialog.highlighted().label, "2160p");
    }

    #[test]
    fn esc_closes_without_choosing() {
        assert_eq!(press(&mut dialog(), KeyCode::Esc), DialogResult::Close);
    }

    #[test]
    fn estimated_sizes_have_a_tilde_and_4k_is_marked() {
        let choices = choosing_summary("dl_1").video.unwrap().choices;
        let text = |i: usize| row_text(&choices[i], 10);
        assert!(text(0).contains("~57.0 MiB"), "{}", text(0));
        assert!(text(0).ends_with("(4K)"), "{}", text(0));
        assert!(text(1).contains("22.0 MiB") && !text(1).contains('~'));
        assert!(!text(1).contains("4K"));
        assert!(text(2).starts_with("1080p"), "{}", text(2));
        assert!(text(2).contains("MP4"), "{}", text(2));
        assert_eq!(text(5), "360p        MP4", "an unknown size is blank");
        assert!(text(6).contains("M4A"), "{}", text(6));
    }

    #[test]
    fn folder_and_name_are_checked_like_the_add_dialog() {
        let mut dialog = dialog();
        // Tab goes to Folder, then Name.
        press(&mut dialog, KeyCode::Tab);
        assert_eq!(dialog.focused(), QualityFocus::Folder);
        dialog.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        for c in "Videos".chars() {
            press(&mut dialog, KeyCode::Char(c));
        }
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert_eq!(
            dialog.error(),
            Some("Enter a full path, such as ~/Downloads")
        );
        assert!(!dialog.is_pending());

        // A full path is sent; so is a changed name.
        dialog.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        dialog.handle_event(&Event::Paste("~/Videos".into()));
        press(&mut dialog, KeyCode::Tab);
        assert_eq!(dialog.focused(), QualityFocus::Name);
        dialog.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert_eq!(dialog.error(), Some("Enter a file name"));
        assert_eq!(dialog.focused(), QualityFocus::Name);

        dialog.handle_event(&Event::Paste("My clip".into()));
        let params = chosen(press(&mut dialog, KeyCode::Enter));
        let home = crate::dirs::home_dir().expect("a home folder");
        assert_eq!(params.destination, Some(home.join("Videos")));
        assert_eq!(params.filename.as_deref(), Some("My clip"));
    }

    #[test]
    fn the_fields_start_from_the_download_and_are_sent_only_when_changed() {
        let dialog = dialog();
        assert_eq!(dialog.folder_text(), "/downloads");
        assert_eq!(dialog.name_text(), "Rust in 100 Seconds");
        let mut dialog = dialog;
        let params = chosen(press(&mut dialog, KeyCode::Enter));
        assert_eq!((params.destination, params.filename), (None, None));
    }

    #[test]
    fn a_failed_choice_shows_in_the_dialog_and_keeps_it_open() {
        let mut dialog = dialog();
        chosen(press(&mut dialog, KeyCode::Enter));
        dialog.set_error(&crate::tui::daemon_link::ActionError {
            message: "no such folder".into(),
            code: Some(CODE_INVALID_PATH),
            not_added: Vec::new(),
        });
        assert!(!dialog.is_pending());
        assert_eq!(dialog.error(), Some("no such folder"));
        assert_eq!(dialog.focused(), QualityFocus::Folder);
        chosen(press(&mut dialog, KeyCode::Enter));
    }

    #[test]
    fn only_esc_works_while_waiting_for_the_daemon() {
        let mut dialog = dialog();
        chosen(press(&mut dialog, KeyCode::Enter));
        assert_eq!(press(&mut dialog, KeyCode::Down), DialogResult::Used);
        assert_eq!(dialog.highlighted().label, "1080p");
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert_eq!(press(&mut dialog, KeyCode::Esc), DialogResult::Close);
    }

    fn draw(width: u16, height: u16) -> (String, Vec<(Rect, DialogTarget)>) {
        let mut app = sample_app();
        app.dialogs.push(Dialog::Quality(Box::new(dialog())));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        (screen_text(&terminal), app.hits.dialog.clone())
    }

    #[test]
    fn every_row_shows_whole_at_80x24_and_100x32() {
        for (width, height) in [(80, 24), (100, 32)] {
            let (text, hits) = draw(width, height);
            for want in [
                "Rust in 100 Seconds",
                "2160p",
                "~57.0 MiB (4K)",
                "1440p",
                "> 1080p",
                "720p",
                "480p",
                "360p",
                "Audio only",
                "Folder",
                "Name",
                "Enter Download",
            ] {
                assert!(text.contains(want), "{width}x{height} lacks {want}: {text}");
            }
            let rows = hits
                .iter()
                .filter(|(_, t)| matches!(t, DialogTarget::Row(_)))
                .count();
            assert_eq!(rows, 7, "{width}x{height}");
        }
    }

    #[test]
    fn a_short_terminal_scrolls_to_keep_the_highlighted_row_in_view() {
        let mut app = sample_app();
        let mut dialog = dialog();
        for _ in 0..4 {
            press(&mut dialog, KeyCode::Down);
        }
        assert_eq!(dialog.highlighted().label, "Audio only");
        app.dialogs.push(Dialog::Quality(Box::new(dialog)));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("> Audio only"), "{text}");
        assert!(!text.contains("2160p"), "{text}");
    }

    #[test]
    fn clicking_a_row_highlights_it_and_a_field_focuses_it() {
        let mut dialog = dialog();
        dialog.click(DialogTarget::Row(0));
        assert_eq!(dialog.highlighted().label, "2160p");
        assert_eq!(dialog.focused(), QualityFocus::List);
        dialog.click(DialogTarget::Field(1));
        assert_eq!(dialog.focused(), QualityFocus::Name);
        dialog.click(DialogTarget::Field(0));
        assert_eq!(dialog.focused(), QualityFocus::Folder);
        let result = dialog.click(DialogTarget::Key(KeyCode::Enter));
        assert_eq!(chosen(result).choice_id, "2160-webm");
    }
}
