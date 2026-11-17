use super::*;
use crate::ipc::protocol::{
    DaemonLimits, DownloadDefaults, SetDefaultsParams, SetLimitsParams, CODE_INVALID_PARAMS,
};
use crate::ipc::protocol::{ToolInfo, ToolPhase};
use crate::tools::release::now_unix;
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
            detail: None,
        },
        ToolInfo {
            tool: ToolName::Ffmpeg,
            installed: None,
            previous: None,
            last_checked_unix: None,
            phase: ToolPhase::Downloading,
            downloaded_bytes: 12_000_000,
            total_bytes: Some(24_000_000),
            error: None,
            detail: None,
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
            detail: None,
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
        text.contains("Getting ffmpeg: 12/24 MB"),
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
