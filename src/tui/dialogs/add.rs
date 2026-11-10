//! The Add dialog: one or more links, with folder, file name, connections
//! and speed cap starting from the daemon's defaults.

use super::{centered, draw_box, DialogResult, DialogTarget};
use crate::engine::MAX_CONNECTIONS;
use crate::ipc::protocol::{
    AddDownloadParams, DownloadDefaults, CODE_DESTINATION_EXISTS, CODE_INVALID_PATH,
    CODE_PERMISSION_DENIED,
};
use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::format::format_speed_cap;
use crate::tui::theme;
use crate::tui::widgets::text_field::TextField;
use crate::units::{parse_count, parse_speed};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const ONLY_HTTP: &str = "Only http and https links can be downloaded";
const NO_URL: &str = "Paste or type a link to download";
const DIALOG_WIDTH: u16 = 76;
/// Borders, the message row, two rows per field and the buttons.
const DIALOG_HEIGHT: u16 = 2 + 1 + 2 * AddField::ALL.len() as u16 + 1;
const LABEL_WIDTH: u16 = 13;

/// The Add dialog's fields, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddField {
    Url,
    Folder,
    Name,
    Connections,
    Speed,
}

impl AddField {
    const ALL: [AddField; 5] = [
        AddField::Url,
        AddField::Folder,
        AddField::Name,
        AddField::Connections,
        AddField::Speed,
    ];

    fn label(self) -> &'static str {
        match self {
            AddField::Url => "URL",
            AddField::Folder => "Folder",
            AddField::Name => "File name",
            AddField::Connections => "Connections",
            AddField::Speed => "Speed cap",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            AddField::Connections => "1 to 16",
            AddField::Speed => "500K, 2M, or off",
            _ => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AddDialog {
    fields: [TextField; 5],
    /// Links pasted as several lines. Empty while the URL field holds one link.
    urls: Vec<String>,
    focus: AddField,
    errors: [Option<String>; 5],
    title_error: Option<String>,
    pending: bool,
}

impl AddDialog {
    /// A dialog whose folder, connections and speed start from `defaults`,
    /// with `paste` already in the URL field.
    pub fn new(defaults: Option<&DownloadDefaults>, paste: Option<&str>) -> Self {
        let (folder, connections, speed) = defaults
            .map(|d| {
                (
                    d.folder.display().to_string(),
                    d.connections.to_string(),
                    format_speed_cap(d.max_bytes_per_sec),
                )
            })
            .unwrap_or_default();
        let mut dialog = Self {
            fields: [
                TextField::default(),
                TextField::new(&folder),
                TextField::default(),
                TextField::new(&connections),
                TextField::new(&speed),
            ],
            urls: Vec::new(),
            focus: AddField::Url,
            errors: Default::default(),
            title_error: None,
            pending: false,
        };
        if let Some(text) = paste {
            dialog.paste_urls(text);
        }
        dialog
    }

    pub fn focused(&self) -> AddField {
        self.focus
    }

    pub fn field_text(&self, field: AddField) -> &str {
        self.fields[field as usize].text()
    }

    pub fn error(&self, field: AddField) -> Option<&str> {
        self.errors[field as usize].as_deref()
    }

    pub fn title_error(&self) -> Option<&str> {
        self.title_error.as_deref()
    }

    /// Every link the dialog would add.
    pub fn urls(&self) -> Vec<String> {
        if self.urls.is_empty() {
            parse_urls(self.field_text(AddField::Url))
        } else {
            self.urls.clone()
        }
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
                    AddField::Url => self.paste_urls(text),
                    field => self.fields[field as usize].paste(text),
                }
                DialogResult::Used
            }
            _ => DialogResult::Used,
        }
    }

    pub fn click(&mut self, target: DialogTarget) -> DialogResult {
        match target {
            DialogTarget::Field(i) => {
                if let Some(field) = AddField::ALL.get(i) {
                    self.focus = *field;
                }
                DialogResult::Used
            }
            DialogTarget::Key(code) => {
                self.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            }
        }
    }

    fn handle_key(&mut self, key: &KeyEvent) -> DialogResult {
        match key.code {
            KeyCode::Esc => return DialogResult::Close,
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab | KeyCode::Down => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(AddField::ALL.len() - 1),
            _ if self.focus == AddField::Url && !self.urls.is_empty() => {
                // Editing a list of links starts over with one typed link.
                let mut fresh = TextField::default();
                if fresh.handle_key(key) {
                    self.urls.clear();
                    self.fields[AddField::Url as usize] = fresh;
                }
            }
            _ => {
                self.fields[self.focus as usize].handle_key(key);
            }
        }
        DialogResult::Used
    }

    fn move_focus(&mut self, step: usize) {
        let next = (self.focus as usize + step) % AddField::ALL.len();
        self.focus = AddField::ALL[next];
    }

    /// One pasted line goes into the field; several become a list of links.
    fn paste_urls(&mut self, text: &str) {
        let pasted = parse_urls(text);
        if pasted.len() <= 1 && self.urls.is_empty() {
            self.fields[AddField::Url as usize].paste(text.trim());
            return;
        }
        let mut all = self.urls();
        all.extend(pasted);
        self.urls = all;
        self.fields[AddField::Url as usize] = TextField::default();
    }

    fn submit(&mut self) -> DialogResult {
        self.errors = Default::default();
        self.title_error = None;
        match self.params() {
            Ok(params) => {
                self.pending = true;
                DialogResult::Submit(Action::Add(params))
            }
            Err((field, message)) => {
                self.errors[field as usize] = Some(message);
                self.focus = field;
                DialogResult::Used
            }
        }
    }

    /// The text of `field`, or `None` when it is blank.
    fn optional(&self, field: AddField) -> Option<&str> {
        Some(self.field_text(field).trim()).filter(|t| !t.is_empty())
    }

    /// Check every field and build one `download.add` per link.
    fn params(&self) -> Result<Vec<AddDownloadParams>, (AddField, String)> {
        let urls = self.urls();
        if urls.is_empty() {
            return Err((AddField::Url, NO_URL.to_string()));
        }
        if !urls.iter().all(|u| is_http(u)) {
            return Err((AddField::Url, ONLY_HTTP.to_string()));
        }
        // The daemon runs in another folder, so send it a full path.
        let destination = self
            .optional(AddField::Folder)
            .map(std::path::absolute)
            .transpose()
            .map_err(|e| (AddField::Folder, format!("Can't use this folder: {e}")))?;
        let connections = self
            .optional(AddField::Connections)
            .map(|t| parse_count(t, 1, MAX_CONNECTIONS))
            .transpose()
            .map_err(|e| (AddField::Connections, e))?;
        let max_bytes_per_sec = self
            .optional(AddField::Speed)
            .map(parse_speed)
            .transpose()
            .map_err(|e| (AddField::Speed, e))?;
        let filename = match urls.len() {
            1 => self.optional(AddField::Name).map(String::from),
            _ => None,
        };
        Ok(urls
            .into_iter()
            .map(|url| AddDownloadParams {
                url,
                destination: destination.clone(),
                filename: filename.clone(),
                connections: connections.map(|n| n as usize),
                max_bytes_per_sec,
            })
            .collect())
    }

    /// Show a failed submit. Links that were added leave the list; folder
    /// problems show under Folder and the rest under the title.
    pub fn set_error(&mut self, err: &ActionError) {
        self.pending = false;
        match err.not_added.as_slice() {
            [] => {}
            [one] => {
                self.urls.clear();
                self.fields[AddField::Url as usize] = TextField::new(one);
            }
            many => {
                self.urls = many.to_vec();
                self.fields[AddField::Url as usize] = TextField::default();
            }
        }
        let folder_problem = matches!(
            err.code,
            Some(CODE_INVALID_PATH | CODE_PERMISSION_DENIED | CODE_DESTINATION_EXISTS)
        );
        if folder_problem {
            self.errors[AddField::Folder as usize] = Some(err.message.clone());
        } else {
            self.title_error = Some(err.message.clone());
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) -> Vec<(Rect, DialogTarget)> {
        let inner = draw_box(
            frame,
            centered(area, DIALOG_WIDTH, DIALOG_HEIGHT),
            "Add a download",
        );
        let mut hits = Vec::new();
        let message = match (&self.title_error, self.pending) {
            (Some(err), _) => Span::styled(err.clone(), theme::ERROR),
            (None, true) => Span::styled("Adding…", theme::MUTED),
            (None, false) => Span::styled("Tab moves between fields", theme::DIM),
        };
        put(frame, inner, 0, Line::from(message));
        for (i, field) in AddField::ALL.into_iter().enumerate() {
            let row = 1 + 2 * i as u16;
            if let Some(rect) = self.render_field(frame, inner, row, field) {
                hits.push((rect, DialogTarget::Field(i)));
            }
            let under = match &self.errors[i] {
                Some(err) => Span::styled(err.clone(), theme::ERROR),
                None => Span::styled(field.hint(), theme::DIM),
            };
            let indent = Span::raw(" ".repeat(LABEL_WIDTH as usize));
            put(frame, inner, row + 1, Line::from(vec![indent, under]));
        }
        hits.extend(render_buttons(frame, inner, DIALOG_HEIGHT - 3));
        hits
    }

    /// Draw one field's label and value; returns the value's area if it fits.
    fn render_field(
        &self,
        frame: &mut Frame,
        inner: Rect,
        row: u16,
        field: AddField,
    ) -> Option<Rect> {
        if row >= inner.height || inner.width <= LABEL_WIDTH {
            return None;
        }
        let y = inner.y + row;
        let label = Rect::new(inner.x, y, LABEL_WIDTH, 1);
        frame.render_widget(Paragraph::new(field.label()).style(theme::MUTED), label);
        let value = Rect::new(inner.x + LABEL_WIDTH, y, inner.width - LABEL_WIDTH, 1);
        let focused = field == self.focus && !self.pending;
        let style = if focused {
            theme::FIELD_FOCUSED
        } else {
            theme::FIELD
        };
        let several = self.urls.len() > 1;
        match field {
            AddField::Url if several => {
                let text = format!("{} URLs", self.urls.len());
                frame.render_widget(Paragraph::new(text).style(style), value);
            }
            AddField::Name if several => {
                let text = "Each file keeps the server's name";
                frame.render_widget(Paragraph::new(text).style(theme::DIM), value);
            }
            _ => self.fields[field as usize].render(frame, value, focused),
        }
        Some(value)
    }
}

/// "[ Enter Add ]  [ Esc Cancel ]" on `row`, each acting as its key.
fn render_buttons(frame: &mut Frame, inner: Rect, row: u16) -> Vec<(Rect, DialogTarget)> {
    let mut hits = Vec::new();
    if row >= inner.height {
        return hits;
    }
    let mut x = inner.x + LABEL_WIDTH;
    for (label, code, style) in [
        ("[ Enter Add ]", KeyCode::Enter, theme::BUTTON_ADD),
        ("[ Esc Cancel ]", KeyCode::Esc, theme::BUTTON),
    ] {
        let width = label.len() as u16;
        if x + width > inner.right() {
            break;
        }
        let rect = Rect::new(x, inner.y + row, width, 1);
        frame.render_widget(Paragraph::new(label).style(style), rect);
        hits.push((rect, DialogTarget::Key(code)));
        x += width + 2;
    }
    hits
}

/// Draw `line` on `row` of `inner`, if that row exists.
fn put(frame: &mut Frame, inner: Rect, row: u16, line: Line) {
    if row < inner.height {
        let rect = Rect::new(inner.x, inner.y + row, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(Style::new()), rect);
    }
}

fn is_http(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// The links in pasted text: one per line, trimmed, blank lines dropped.
pub fn parse_urls(text: &str) -> Vec<String> {
    text.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::client::ClientError;
    use crate::ipc::protocol::{CODE_DUPLICATE_DOWNLOAD, CODE_INVALID_PATH};
    use crate::tui::daemon_link::ActionError;
    use crate::tui::dialogs::{Dialog, DialogTarget};
    use crate::tui::state::App;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn defaults() -> DownloadDefaults {
        DownloadDefaults {
            folder: "/dl".into(),
            connections: 4,
            max_bytes_per_sec: 512 * 1024,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press(dialog: &mut AddDialog, code: KeyCode) -> DialogResult {
        dialog.handle_event(&key(code))
    }

    fn type_text(dialog: &mut AddDialog, text: &str) {
        for c in text.chars() {
            press(dialog, KeyCode::Char(c));
        }
    }

    fn submitted(result: DialogResult) -> Vec<AddDownloadParams> {
        match result {
            DialogResult::Submit(Action::Add(params)) => params,
            other => panic!("expected an Add submit, got {other:?}"),
        }
    }

    fn focus(dialog: &mut AddDialog, field: AddField) {
        while dialog.focused() != field {
            press(dialog, KeyCode::Tab);
        }
    }

    #[test]
    fn a_paste_then_enter_submits_with_the_defaults() {
        let mut dialog = AddDialog::new(Some(&defaults()), None);
        dialog.handle_event(&Event::Paste("https://example.com/a.iso".into()));
        let params = submitted(press(&mut dialog, KeyCode::Enter));
        assert_eq!(
            params,
            vec![AddDownloadParams {
                url: "https://example.com/a.iso".into(),
                destination: Some("/dl".into()),
                filename: None,
                connections: Some(4),
                max_bytes_per_sec: Some(512 * 1024),
            }]
        );
        assert!(dialog.is_pending());
    }

    #[test]
    fn pasted_crlf_lines_become_clean_urls() {
        assert_eq!(
            parse_urls("https://a/1\r\n  https://a/2 \r\n\r\n\thttps://a/3\t"),
            vec!["https://a/1", "https://a/2", "https://a/3"]
        );
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://a/1\r\nhttps://a/2\r\n"));
        assert_eq!(dialog.urls(), vec!["https://a/1", "https://a/2"]);
        focus(&mut dialog, AddField::Name);
        type_text(&mut dialog, "ignored.bin");
        let params = submitted(press(&mut dialog, KeyCode::Enter));
        let urls: Vec<_> = params.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a/1", "https://a/2"]);
        assert!(params.iter().all(|p| p.filename.is_none()));
    }

    #[test]
    fn a_non_http_url_is_rejected_under_the_url_field() {
        let mut dialog = AddDialog::new(Some(&defaults()), Some("ftp://example.com/a"));
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert_eq!(
            dialog.error(AddField::Url),
            Some("Only http and https links can be downloaded")
        );
        assert!(!dialog.is_pending());
    }

    #[test]
    fn a_bad_connections_value_is_rejected_under_its_field() {
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://example.com/a"));
        focus(&mut dialog, AddField::Connections);
        dialog.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        type_text(&mut dialog, "40");
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert_eq!(
            dialog.error(AddField::Connections),
            Some("Enter a whole number from 1 to 16")
        );
    }

    #[test]
    fn a_bad_speed_is_rejected_under_its_field() {
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://example.com/a"));
        focus(&mut dialog, AddField::Speed);
        type_text(&mut dialog, "x");
        assert_eq!(press(&mut dialog, KeyCode::Enter), DialogResult::Used);
        assert!(dialog.error(AddField::Speed).is_some());
    }

    #[test]
    fn a_failed_submit_keeps_the_typed_values() {
        let mut app = App::default();
        let mut dialog = AddDialog::new(Some(&defaults()), None);
        type_text(&mut dialog, "https://example.com/typed.iso");
        submitted(press(&mut dialog, KeyCode::Enter));
        app.dialogs.push(Dialog::Add(dialog));

        app.apply_outcome(Err(ActionError::from(ClientError::NotRunning)));
        let Some(Dialog::Add(dialog)) = app.dialogs.last() else {
            panic!("the dialog closed");
        };
        assert_eq!(
            dialog.field_text(AddField::Url),
            "https://example.com/typed.iso"
        );
        assert_eq!(dialog.field_text(AddField::Folder), "/dl");
        assert_eq!(dialog.title_error(), Some("rdmd is not running"));
        assert!(!dialog.is_pending());
        assert_eq!(app.notice, None, "the error shows in the dialog");
    }

    #[test]
    fn a_successful_submit_closes_the_dialog_and_selects_the_download() {
        let mut app = App::default();
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://example.com/a"));
        submitted(press(&mut dialog, KeyCode::Enter));
        app.dialogs.push(Dialog::Add(dialog));
        app.apply_outcome(Ok(crate::tui::daemon_link::Outcome {
            message: "Added dl_1 (queued)".into(),
            select: Some("dl_1".into()),
        }));
        assert!(app.dialogs.is_empty());
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
    }

    #[test]
    fn only_the_urls_that_failed_stay_in_the_dialog() {
        let mut dialog = AddDialog::new(
            Some(&defaults()),
            Some("https://a/1\nhttps://a/2\nhttps://a/3"),
        );
        submitted(press(&mut dialog, KeyCode::Enter));
        dialog.set_error(&ActionError {
            message: "1 of 3 not added: already downloading".into(),
            code: Some(CODE_DUPLICATE_DOWNLOAD),
            not_added: vec!["https://a/2".into()],
        });
        assert_eq!(dialog.urls(), vec!["https://a/2"]);
        assert_eq!(
            dialog.title_error(),
            Some("1 of 3 not added: already downloading")
        );
    }

    #[test]
    fn folder_errors_show_under_the_folder() {
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://a/1"));
        submitted(press(&mut dialog, KeyCode::Enter));
        dialog.set_error(&ActionError {
            message: "no such folder".into(),
            code: Some(CODE_INVALID_PATH),
            not_added: vec!["https://a/1".into()],
        });
        assert_eq!(dialog.error(AddField::Folder), Some("no such folder"));
        assert_eq!(dialog.title_error(), None);
    }

    #[test]
    fn esc_closes_the_dialog_without_adding() {
        let mut dialog = AddDialog::new(Some(&defaults()), Some("https://a/1"));
        assert_eq!(press(&mut dialog, KeyCode::Esc), DialogResult::Close);
    }

    #[test]
    fn tab_and_shift_tab_move_between_fields() {
        let mut dialog = AddDialog::new(Some(&defaults()), None);
        assert_eq!(dialog.focused(), AddField::Url);
        press(&mut dialog, KeyCode::Tab);
        assert_eq!(dialog.focused(), AddField::Folder);
        press(&mut dialog, KeyCode::BackTab);
        press(&mut dialog, KeyCode::BackTab);
        assert_eq!(dialog.focused(), AddField::Speed, "wraps around");
    }

    #[test]
    fn clicking_a_field_focuses_it() {
        let mut add = AddDialog::new(Some(&defaults()), None);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| hits = add.render(f, Rect::new(0, 0, 100, 30)))
            .unwrap();
        let (_, target) = hits
            .iter()
            .find(|(_, t)| *t == DialogTarget::Field(AddField::Name as usize))
            .expect("the name field is clickable");
        add.click(*target);
        assert_eq!(add.focused(), AddField::Name);
        type_text(&mut add, "x.bin");
        assert_eq!(add.field_text(AddField::Name), "x.bin");
    }
}
