//! The Settings screen: the daemon's limits and the defaults for new
//! downloads, each field edited in place and saved straight to the daemon.

use crate::daemon::core::MAX_ACTIVE_LIMIT;
use crate::dirs::{full_folder, home_dir};
use crate::engine::MAX_CONNECTIONS;
use crate::ipc::protocol::{SetDefaultsParams, SetLimitsParams, ToolName, ToolPhase};
use crate::tools::display::{age_text, state_text};
use crate::tools::release::now_unix;
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
            let row = Rect::new(inner.x, y, inner.width, 1);
            render_field(frame, app, field, row);
            hits.push((row, field));
            y += 1;
            if let Some(tool) = field.tool() {
                render_tool_notes(
                    frame,
                    app,
                    tool,
                    field == app.settings.selected,
                    inner,
                    &mut y,
                );
            }
            if field == app.settings.selected && app.settings.is_editing() {
                let under = match &app.settings.error {
                    Some(err) => Span::styled(err.clone(), theme::ERROR),
                    None => Span::styled(
                        format!("{} · Enter Save · Esc Cancel", field.hint()),
                        theme::DIM,
                    ),
                };
                let indent = Span::raw(" ".repeat((INDENT + LABEL_WIDTH) as usize));
                put_line(frame, inner, &mut y, Line::from(vec![indent, under]));
            }
        }
    }
    app.hits.settings = hits;
}

/// Draw `line` on row `y` of `inner` if it fits, and move `y` down.
fn put_line(frame: &mut Frame, inner: Rect, y: &mut u16, line: Line) {
    if *y < inner.bottom() {
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, *y, inner.width, 1));
    }
    *y += 1;
}

/// Under a tool's row: why its last attempt failed, and, when it is the
/// selected row, which keys act on it.
fn render_tool_notes(
    frame: &mut Frame,
    app: &App,
    tool: ToolName,
    selected: bool,
    inner: Rect,
    y: &mut u16,
) {
    let indent = || Span::raw(" ".repeat((INDENT + LABEL_WIDTH) as usize));
    let failure = app
        .tools
        .iter()
        .find(|i| i.tool == tool && i.phase == ToolPhase::Idle)
        .and_then(|i| i.error.clone());
    if let Some(error) = failure {
        let line = Line::from(vec![indent(), Span::styled(error, theme::ERROR)]);
        put_line(frame, inner, y, line);
    }
    if selected {
        let hint = Span::styled("u Update · b Go back one version", theme::DIM);
        put_line(frame, inner, y, Line::from(vec![indent(), hint]));
    }
}

/// What a tool row shows after its name: version, last check, previous
/// version, or the progress of what it is doing now.
fn tool_value(app: &App, tool: ToolName, style: Style) -> Line<'static> {
    let Some(info) = app.tools.iter().find(|i| i.tool == tool) else {
        return Line::from(Span::styled("—", style));
    };
    if info.phase != ToolPhase::Idle {
        return Line::from(Span::styled(state_text(info), style));
    }
    let Some(version) = &info.installed else {
        return Line::from(Span::styled("not installed", style));
    };
    let mut spans = vec![Span::styled(version.clone(), style)];
    let mut details = format!("  checked {}", age_text(info.last_checked_unix, now_unix()));
    if let Some(previous) = &info.previous {
        details.push_str(&format!("  previous {previous}"));
    }
    spans.push(Span::styled(details, theme::DIM));
    Line::from(spans)
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
mod tests {
    use super::*;
    use crate::ipc::protocol::ToolInfo;
    use crate::ipc::protocol::{
        DaemonLimits, DownloadDefaults, SetDefaultsParams, SetLimitsParams, CODE_INVALID_PARAMS,
    };
    use crate::tui::action::Action;
    use crate::tui::daemon_link::ActionError;
    use crate::tui::screens;
    use crate::tui::state::test_support::screen_text;
    use crate::tui::state::Screen;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn click(rect: Rect) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn send(app: &mut App, event: Event) -> Option<Action> {
        screens::handle_event(app, &event)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            assert_eq!(send(app, key(KeyCode::Char(c))), None);
        }
    }

    fn draw(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| screens::render(f, app)).unwrap();
        screen_text(&terminal)
    }

    fn settings_app() -> App {
        App {
            connected: true,
            screen: Screen::Settings,
            limits: Some(DaemonLimits {
                max_active: 3,
                max_bytes_per_sec: 0,
            }),
            defaults: Some(DownloadDefaults {
                folder: PathBuf::from("/data/downloads"),
                connections: 8,
                max_bytes_per_sec: 0,
            }),
            ..App::default()
        }
    }

    #[test]
    fn the_screen_shows_both_groups_with_their_values() {
        let mut app = settings_app();
        let text = draw(&mut app);
        for want in [
            "Limits",
            "Downloads at once",
            "Total speed cap",
            "New downloads",
            "Folder",
            "/data/downloads",
            "Connections",
            "Speed cap per download",
            "unlimited",
            "Tools",
            "yt-dlp",
            "ffmpeg",
            "deno",
        ] {
            assert!(text.contains(want), "missing {want}:\n{text}");
        }
    }

    #[test]
    fn u_and_b_dispatch_tool_actions_and_enter_is_ignored_on_tools() {
        let mut app = settings_app();
        for _ in 0..5 {
            send(&mut app, key(KeyCode::Down));
        }
        assert_eq!(app.settings.selected, SettingsField::YtDlp);

        assert_eq!(send(&mut app, key(KeyCode::Enter)), None);
        assert!(!app.settings.is_editing());

        assert_eq!(
            send(&mut app, key(KeyCode::Char('u'))),
            Some(Action::UpdateTool(ToolName::YtDlp))
        );
        assert_eq!(
            send(&mut app, key(KeyCode::Char('b'))),
            Some(Action::RollbackTool(ToolName::YtDlp))
        );

        send(&mut app, key(KeyCode::Down));
        assert_eq!(app.settings.selected, SettingsField::Ffmpeg);
        assert_eq!(
            send(&mut app, key(KeyCode::Char('u'))),
            Some(Action::UpdateTool(ToolName::Ffmpeg))
        );
        assert_eq!(
            send(&mut app, key(KeyCode::Char('b'))),
            Some(Action::RollbackTool(ToolName::Ffmpeg))
        );
    }

    #[test]
    fn tool_row_shows_version_previous_state_and_failure_note() {
        let mut app = settings_app();
        app.tools = vec![
            ToolInfo {
                tool: ToolName::YtDlp,
                installed: Some("2026.10.26".into()),
                previous: Some("2026.10.15".into()),
                last_checked_unix: Some(now_unix().saturating_sub(60)),
                phase: ToolPhase::Idle,
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
            },
            ToolInfo {
                tool: ToolName::Ffmpeg,
                installed: None,
                previous: None,
                last_checked_unix: None,
                phase: ToolPhase::Downloading,
                downloaded_bytes: 12 * 1024 * 1024,
                total_bytes: Some(24 * 1024 * 1024),
                error: None,
            },
            ToolInfo {
                tool: ToolName::Deno,
                installed: Some("2.9.8".into()),
                previous: None,
                last_checked_unix: Some(now_unix().saturating_sub(120)),
                phase: ToolPhase::Idle,
                downloaded_bytes: 0,
                total_bytes: None,
                error: Some("checksum mismatch".into()),
            },
        ];

        for _ in 0..7 {
            send(&mut app, key(KeyCode::Down));
        }
        assert_eq!(app.settings.selected, SettingsField::Deno);

        let text = draw(&mut app);
        assert!(
            text.contains("2026.10.26"),
            "shows yt-dlp installed version: {text}"
        );
        assert!(
            text.contains("previous 2026.10.15"),
            "shows yt-dlp previous version: {text}"
        );
        assert!(
            text.contains("Getting ffmpeg: 12/24 MiB"),
            "shows ffmpeg downloading: {text}"
        );
        assert!(
            text.contains("2.9.8"),
            "shows deno installed version: {text}"
        );
        assert!(
            text.contains("checksum mismatch"),
            "shows deno error note: {text}"
        );
        assert!(
            text.contains("u Update · b Go back one version"),
            "shows hint under selected tool: {text}"
        );
    }

    #[test]
    fn enter_edits_and_saves_one_field() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Down));
        assert_eq!(app.settings.selected, SettingsField::Folder);
        assert_eq!(send(&mut app, key(KeyCode::Enter)), None);
        assert_eq!(
            app.settings.editing.as_ref().map(|f| f.text()),
            Some("/data/downloads"),
            "editing starts from the current value"
        );
        let folder = std::env::temp_dir();
        send(&mut app, ctrl('u'));
        send(&mut app, Event::Paste(folder.display().to_string()));
        assert_eq!(
            send(&mut app, key(KeyCode::Enter)),
            Some(Action::SetDefaults(SetDefaultsParams {
                folder: Some(folder),
                connections: None,
                max_bytes_per_sec: None,
            }))
        );
        app.apply_outcome(Ok(Default::default()));
        assert!(app.settings.editing.is_none(), "saving ends editing");
    }

    #[test]
    fn the_total_speed_cap_saves_as_a_limit() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Enter));
        send(&mut app, ctrl('u'));
        type_text(&mut app, "2M");
        assert_eq!(
            send(&mut app, key(KeyCode::Enter)),
            Some(Action::SetLimits(SetLimitsParams {
                max_active: None,
                max_bytes_per_sec: Some(2 * 1024 * 1024),
            }))
        );
    }

    #[test]
    fn esc_while_editing_reverts() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Enter));
        type_text(&mut app, "9");
        assert_eq!(send(&mut app, key(KeyCode::Esc)), None);
        assert!(app.settings.editing.is_none());
        assert_eq!(app.screen, Screen::Settings, "Esc only stopped editing");
        assert!(draw(&mut app).contains("Downloads at once         3"));
        send(&mut app, key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Downloads, "a second Esc goes back");
    }

    #[test]
    fn global_keys_are_typed_into_the_field_being_edited() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Enter));
        send(&mut app, ctrl('u'));
        type_text(&mut app, "/qslQ?");
        assert_eq!(app.screen, Screen::Settings);
        assert!(!app.help_open && app.dialogs.is_empty());
        assert_eq!(
            app.settings.editing.as_ref().map(|f| f.text()),
            Some("/qslQ?")
        );
        assert_eq!(send(&mut app, ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn a_daemon_rejection_shows_under_the_field() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Enter));
        assert!(send(&mut app, key(KeyCode::Enter)).is_some());
        app.apply_outcome(Err(ActionError {
            message: "folder: must be a full path".into(),
            code: Some(CODE_INVALID_PARAMS),
            not_added: vec![],
        }));
        assert_eq!(
            app.settings.error.as_deref(),
            Some("folder: must be a full path")
        );
        assert!(app.settings.editing.is_some(), "editing continues");
        assert!(!app.settings.pending);
        assert!(app.notice.is_none(), "the error is not a footer notice");
        let text = draw(&mut app);
        let field_row = text.lines().position(|l| l.contains("Folder")).unwrap();
        let error_row = text
            .lines()
            .position(|l| l.contains("folder: must be a full path"))
            .expect("error drawn");
        assert_eq!(error_row, field_row + 1, "{text}");
    }

    #[test]
    fn clicking_a_field_selects_it() {
        let mut app = settings_app();
        draw(&mut app);
        let (rect, _) = *app
            .hits
            .settings
            .iter()
            .find(|(_, f)| *f == SettingsField::Connections)
            .expect("Connections is clickable");
        send(&mut app, click(rect));
        assert_eq!(app.settings.selected, SettingsField::Connections);
        assert!(app.settings.editing.is_none());
        send(&mut app, click(rect));
        assert!(
            app.settings.editing.is_some(),
            "a click on the selected field edits it"
        );
    }

    #[test]
    fn a_tilde_folder_saves_under_the_home_folder_and_a_relative_one_is_refused() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Down));
        send(&mut app, key(KeyCode::Enter));
        send(&mut app, ctrl('u'));
        type_text(&mut app, "Videos");
        assert_eq!(send(&mut app, key(KeyCode::Enter)), None);
        assert_eq!(
            app.settings.error.as_deref(),
            Some("Enter a full path, such as ~/Downloads")
        );

        send(&mut app, ctrl('u'));
        type_text(&mut app, "~/Videos");
        let home = crate::dirs::home_dir().expect("a home folder");
        assert_eq!(
            send(&mut app, key(KeyCode::Enter)),
            Some(Action::SetDefaults(SetDefaultsParams {
                folder: Some(home.join("Videos")),
                ..Default::default()
            }))
        );
    }

    #[test]
    fn limits_reject_zero_downloads_at_once() {
        let mut app = settings_app();
        send(&mut app, key(KeyCode::Enter));
        send(&mut app, ctrl('u'));
        type_text(&mut app, "0");
        assert_eq!(send(&mut app, key(KeyCode::Enter)), None);
        assert_eq!(
            app.settings.error.as_deref(),
            Some("Enter a whole number from 1 to 32")
        );
        assert!(app.settings.editing.is_some());
    }

    #[test]
    fn a_bad_connection_count_is_rejected() {
        let mut app = settings_app();
        for _ in 0..3 {
            send(&mut app, key(KeyCode::Down));
        }
        assert_eq!(app.settings.selected, SettingsField::Connections);
        send(&mut app, key(KeyCode::Enter));
        send(&mut app, ctrl('u'));
        type_text(&mut app, "17");
        assert_eq!(send(&mut app, key(KeyCode::Enter)), None);
        assert_eq!(
            app.settings.error.as_deref(),
            Some("Enter a whole number from 1 to 16")
        );
    }
}
