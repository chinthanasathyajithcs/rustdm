//! The Settings screen: the daemon's limits and the defaults for new
//! downloads, each field edited in place and saved straight to the daemon.

use crate::daemon::core::MAX_ACTIVE_LIMIT;
use crate::dirs::{full_folder, home_dir};
use crate::engine::MAX_CONNECTIONS;
use crate::ipc::protocol::{SetDefaultsParams, SetLimitsParams, ToolName};
use crate::tui::action::Action;
use crate::tui::daemon_link::ActionError;
use crate::tui::format::format_speed_cap;
use crate::tui::screens::hit;
use crate::tui::state::App;
use crate::tui::theme;
use crate::tui::widgets::text_field::TextField;
use crate::units::{parse_count, parse_speed};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use tool_rows::{render_tool_notes, tool_value};

mod tool_rows;

/// Columns before a field's label, and the label column's width.
const INDENT: u16 = 2;
const LABEL_WIDTH: u16 = 26;
const SPEED_HINT: &str = "500K, 2M, or off";

/// The fields, in the order they are listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsField {
    MaxActive,
    TotalSpeed,
    Folder,
    Connections,
    Speed,
    /// The helper tools; these rows are not edited, they take `u` and `b`.
    YtDlp,
    Ffmpeg,
    Deno,
}

/// The groups of fields, in the order they are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Limits,
    NewDownloads,
    Tools,
}

const GROUPS: [(&str, Group); 3] = [
    ("Limits", Group::Limits),
    ("New downloads", Group::NewDownloads),
    ("Tools", Group::Tools),
];

impl SettingsField {
    pub const ALL: [SettingsField; 8] = [
        SettingsField::MaxActive,
        SettingsField::TotalSpeed,
        SettingsField::Folder,
        SettingsField::Connections,
        SettingsField::Speed,
        SettingsField::YtDlp,
        SettingsField::Ffmpeg,
        SettingsField::Deno,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsField::MaxActive => "Downloads at once",
            SettingsField::TotalSpeed => "Total speed cap",
            SettingsField::Folder => "Folder",
            SettingsField::Connections => "Connections",
            SettingsField::Speed => "Speed cap per download",
            SettingsField::YtDlp => "yt-dlp",
            SettingsField::Ffmpeg => "ffmpeg",
            SettingsField::Deno => "deno",
        }
    }

    fn hint(self) -> String {
        match self {
            SettingsField::MaxActive => format!("1 to {MAX_ACTIVE_LIMIT}"),
            SettingsField::Connections => format!("1 to {MAX_CONNECTIONS}"),
            SettingsField::Folder => "A full path; created when a download needs it".into(),
            SettingsField::TotalSpeed | SettingsField::Speed => SPEED_HINT.into(),
            SettingsField::YtDlp | SettingsField::Ffmpeg | SettingsField::Deno => String::new(),
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    fn group(self) -> Group {
        match self {
            SettingsField::MaxActive | SettingsField::TotalSpeed => Group::Limits,
            SettingsField::Folder | SettingsField::Connections | SettingsField::Speed => {
                Group::NewDownloads
            }
            SettingsField::YtDlp | SettingsField::Ffmpeg | SettingsField::Deno => Group::Tools,
        }
    }

    /// The tool this row is about, for the three tool rows.
    pub fn tool(self) -> Option<ToolName> {
        match self {
            SettingsField::YtDlp => Some(ToolName::YtDlp),
            SettingsField::Ffmpeg => Some(ToolName::Ffmpeg),
            SettingsField::Deno => Some(ToolName::Deno),
            _ => None,
        }
    }
}

/// What the Settings screen is doing: the selected field, and the edit in progress.
#[derive(Clone, Debug)]
pub struct SettingsState {
    pub selected: SettingsField,
    /// The field being typed in, if one is.
    pub editing: Option<TextField>,
    /// Why the last save failed, shown under the field.
    pub error: Option<String>,
    /// A save was sent and the daemon has not answered yet.
    pub pending: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            selected: SettingsField::MaxActive,
            editing: None,
            error: None,
            pending: false,
        }
    }
}

impl SettingsState {
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The daemon took the save: stop editing.
    pub fn saved(&mut self) {
        self.stop_editing();
    }

    /// The daemon refused the save: keep editing and show why.
    pub fn set_error(&mut self, err: &ActionError) {
        self.pending = false;
        self.error = Some(err.message.clone());
    }

    fn stop_editing(&mut self) {
        self.editing = None;
        self.error = None;
        self.pending = false;
    }
}

/// The current value of `field` as text, or `None` while the daemon hasn't reported it.
fn current_value(app: &App, field: SettingsField) -> Option<String> {
    let limits = app.limits.as_ref();
    let defaults = app.defaults.as_ref();
    match field {
        SettingsField::MaxActive => limits.map(|l| l.max_active.to_string()),
        SettingsField::TotalSpeed => limits.map(|l| format_speed_cap(l.max_bytes_per_sec)),
        SettingsField::Folder => defaults.map(|d| d.folder.display().to_string()),
        SettingsField::Connections => defaults.map(|d| d.connections.to_string()),
        SettingsField::Speed => defaults.map(|d| format_speed_cap(d.max_bytes_per_sec)),
        SettingsField::YtDlp | SettingsField::Ffmpeg | SettingsField::Deno => None,
    }
}

/// Keys, pastes and clicks on the Settings screen. While a field is being
/// edited, every key but Ctrl+C comes here first, so letters are typed.
pub fn handle_event(app: &mut App, event: &Event) -> Option<Action> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => on_key(app, key),
        Event::Paste(text) => {
            if let Some(field) = app.settings.editing.as_mut() {
                if !app.settings.pending {
                    field.paste(text);
                }
            }
            None
        }
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
            let at = Position::new(mouse.column, mouse.row);
            let field = hit(&app.hits.settings, at)?;
            if field == app.settings.selected {
                if !app.settings.is_editing() {
                    start_editing(app);
                }
            } else {
                app.settings.stop_editing();
                app.settings.selected = field;
            }
            None
        }
        _ => None,
    }
}

fn on_key(app: &mut App, key: &KeyEvent) -> Option<Action> {
    if app.settings.pending {
        return None;
    }
    if let Some(field) = app.settings.editing.as_mut() {
        match key.code {
            KeyCode::Esc => app.settings.stop_editing(),
            KeyCode::Enter => return submit(app),
            _ => {
                field.handle_key(key);
            }
        }
        return None;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => select(app, -1),
        KeyCode::Down | KeyCode::Char('j') => select(app, 1),
        KeyCode::Enter => start_editing(app),
        KeyCode::Char('u') => return app.settings.selected.tool().map(Action::UpdateTool),
        KeyCode::Char('b') => return app.settings.selected.tool().map(Action::RollbackTool),
        _ => {}
    }
    None
}

fn select(app: &mut App, delta: isize) {
    let last = SettingsField::ALL.len() as isize - 1;
    let next = (app.settings.selected.index() as isize + delta).clamp(0, last);
    app.settings.selected = SettingsField::ALL[next as usize];
}

fn start_editing(app: &mut App) {
    // A tool row is not a value to type.
    if app.settings.selected.tool().is_some() {
        return;
    }
    let value = current_value(app, app.settings.selected).unwrap_or_default();
    app.settings.editing = Some(TextField::new(&value));
    app.settings.error = None;
}

/// Check the edited value; send it as one-field params when it is good.
fn submit(app: &mut App) -> Option<Action> {
    let text = app.settings.editing.as_ref()?.text().trim().to_string();
    match parse_field(app.settings.selected, &text) {
        Ok(action) => {
            app.settings.error = None;
            app.settings.pending = true;
            Some(action)
        }
        Err(e) => {
            app.settings.error = Some(e);
            None
        }
    }
}

fn parse_field(field: SettingsField, text: &str) -> Result<Action, String> {
    let limits = |max_active, max_bytes_per_sec| {
        Action::SetLimits(SetLimitsParams {
            max_active,
            max_bytes_per_sec,
        })
    };
    let defaults = |params: SetDefaultsParams| Action::SetDefaults(params);
    Ok(match field {
        SettingsField::MaxActive => {
            let n = parse_count(text, 1, MAX_ACTIVE_LIMIT as u32)?;
            limits(Some(n as usize), None)
        }
        SettingsField::TotalSpeed => limits(None, Some(parse_speed(text)?)),
        SettingsField::Folder => {
            let folder = full_folder(text, home_dir().as_deref())?;
            defaults(SetDefaultsParams {
                folder: Some(folder),
                ..Default::default()
            })
        }
        SettingsField::Connections => defaults(SetDefaultsParams {
            connections: Some(parse_count(text, 1, MAX_CONNECTIONS)?),
            ..Default::default()
        }),
        SettingsField::Speed => defaults(SetDefaultsParams {
            max_bytes_per_sec: Some(parse_speed(text)?),
            ..Default::default()
        }),
        SettingsField::YtDlp | SettingsField::Ffmpeg | SettingsField::Deno => {
            return Err("Tools are updated with u, not edited".to_string())
        }
    })
}

/// Draw both groups and record each field's row in `app.hits.settings`.
pub fn render(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::ALL).title(" Settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut y = inner.y;
    let mut hits = Vec::new();
    for (title, group) in GROUPS {
        if y > inner.y {
            y += 1;
        }
        put_line(
            frame,
            inner,
            &mut y,
            Line::styled(format!(" {title}"), theme::TABLE_HEADER),
        );
        for field in SettingsField::ALL
            .into_iter()
            .filter(|f| f.group() == group)
        {
            if y >= inner.bottom() {
                break;
            }
            hits.push((Rect::new(inner.x, y, inner.width, 1), field));
            render_field_block(frame, app, field, inner, &mut y);
        }
    }
    app.hits.settings = hits;
}

/// One field's row, with what sits under it: a tool's notes, or the edit hint.
fn render_field_block(
    frame: &mut Frame,
    app: &App,
    field: SettingsField,
    inner: Rect,
    y: &mut u16,
) {
    render_field(frame, app, field, Rect::new(inner.x, *y, inner.width, 1));
    *y += 1;
    let selected = field == app.settings.selected;
    if let Some(tool) = field.tool() {
        render_tool_notes(frame, app, tool, selected, inner, y);
    }
    if selected && app.settings.is_editing() {
        let under = match &app.settings.error {
            Some(err) => Span::styled(err.clone(), theme::ERROR),
            None => Span::styled(
                format!("{} · Enter Save · Esc Cancel", field.hint()),
                theme::DIM,
            ),
        };
        let indent = Span::raw(" ".repeat((INDENT + LABEL_WIDTH) as usize));
        put_line(frame, inner, y, Line::from(vec![indent, under]));
    }
}

/// Draw `line` on row `y` of `inner` if it fits, and move `y` down.
pub(super) fn put_line(frame: &mut Frame, inner: Rect, y: &mut u16, line: Line) {
    if *y < inner.bottom() {
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, *y, inner.width, 1));
    }
    *y += 1;
}

fn render_field(frame: &mut Frame, app: &App, field: SettingsField, row: Rect) {
    let selected = field == app.settings.selected;
    let editing = selected && app.settings.is_editing();
    let label = format!(
        "{}{:<width$}",
        " ".repeat(INDENT as usize),
        field.label(),
        width = LABEL_WIDTH as usize
    );
    let label_style = if selected {
        theme::CURRENT
    } else {
        theme::MUTED
    };
    let value_x = (row.x + INDENT + LABEL_WIDTH).min(row.right());
    let value_area = Rect::new(value_x, row.y, row.right() - value_x, 1);
    frame.render_widget(Paragraph::new(Span::styled(label, label_style)), row);
    match (&app.settings.editing, editing) {
        (Some(text), true) => text.render(frame, value_area, !app.settings.pending),
        _ if field.tool().is_some() => {
            let style = if selected {
                theme::SELECTED_ROW
            } else {
                Style::new()
            };
            let line = tool_value(app, field.tool().unwrap_or(ToolName::YtDlp), style);
            frame.render_widget(Paragraph::new(line), value_area);
        }
        _ => {
            let value = current_value(app, field).unwrap_or_else(|| "—".into());
            let style = if selected {
                theme::SELECTED_ROW
            } else {
                Style::new()
            };
            frame.render_widget(Paragraph::new(Span::styled(value, style)), value_area);
        }
    }
}

#[cfg(test)]
mod tests;
