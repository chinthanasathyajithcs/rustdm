//! The TUI's screens. Each reads and changes `App` and returns an `Action`;
//! none of them talks to the daemon. This module owns what every screen
//! shares: the global keys, the header, the footer and the Help overlay.

pub mod downloads;
pub mod help;
pub mod logs;
pub mod settings;

use crate::tui::action::Action;
use crate::tui::dialogs::DialogResult;
use crate::tui::format::format_speed;
use crate::tui::state::{App, Mode, Screen};
use crate::tui::theme;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Separator between screen names in the header and keys in the footer.
const SEPARATOR: &str = " · ";

/// The value whose area contains `at`, if any.
pub(crate) fn hit<T: Clone>(areas: &[(Rect, T)], at: Position) -> Option<T> {
    areas
        .iter()
        .find(|(rect, _)| rect.contains(at))
        .map(|(_, value)| value.clone())
}

/// Turn a terminal event into an action: the global keys first, then the
/// current screen's own handling.
pub fn handle_event(app: &mut App, event: &Event) -> Option<Action> {
    if !app.dialogs.is_empty() {
        return dialog_event(app, event);
    }
    match event {
        // Windows also reports key releases; act on presses only.
        Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
        Event::Mouse(mouse) => {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                let at = Position::new(mouse.column, mouse.row);
                if let Some(screen) = hit(&app.hits.screens, at) {
                    show(app, screen);
                    return None;
                }
            }
            on_screen(app, event)
        }
        _ => on_screen(app, event),
    }
}

/// The top dialog gets every event; Ctrl+C still quits.
fn dialog_event(app: &mut App, event: &Event) -> Option<Action> {
    let result = match event {
        Event::Key(key) if is_ctrl_c(key) => return Some(Action::Quit),
        Event::Mouse(mouse) => {
            let at = Position::new(mouse.column, mouse.row);
            match (mouse.kind, hit(&app.hits.dialog, at)) {
                (MouseEventKind::Down(MouseButton::Left), Some(target)) => {
                    app.dialogs.last_mut()?.click(target)
                }
                _ => DialogResult::Used,
            }
        }
        _ => app.dialogs.last_mut()?.handle_event(event),
    };
    match result {
        DialogResult::Used => None,
        DialogResult::Close => {
            app.dialogs.pop();
            None
        }
        DialogResult::Submit(action) => Some(action),
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

fn handle_key(app: &mut App, key: &KeyEvent) -> Option<Action> {
    if is_ctrl_c(key) {
        return Some(Action::Quit);
    }
    // While the screen is asking for input, keys belong to it.
    if app.mode != Mode::Browse {
        return downloads::handle_event(app, &Event::Key(*key));
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => {
            app.help_open = !app.help_open;
            None
        }
        KeyCode::Esc if app.help_open => {
            app.help_open = false;
            None
        }
        KeyCode::Esc => {
            app.screen = Screen::Downloads;
            None
        }
        // Help covers the screen; only the keys above reach past it.
        _ if app.help_open => None,
        KeyCode::Char('s') => show(app, Screen::Settings),
        KeyCode::Char('l') => show(app, Screen::Logs),
        _ => on_screen(app, &Event::Key(*key)),
    }
}

fn show(app: &mut App, screen: Screen) -> Option<Action> {
    app.screen = screen;
    app.help_open = false;
    None
}

/// Pass an event to the current screen, unless Help covers it.
fn on_screen(app: &mut App, event: &Event) -> Option<Action> {
    if app.help_open {
        return None;
    }
    match app.screen {
        Screen::Downloads => downloads::handle_event(app, event),
        Screen::Settings | Screen::Logs => None,
    }
}

/// Draw the current screen and record what is clickable in `app.hits`.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    app.hits = Default::default();
    app.hits.screens = render_header(frame, app, header);
    match app.screen {
        Screen::Downloads => downloads::render(frame, app, body),
        Screen::Settings => settings::render(frame, app, body),
        Screen::Logs => logs::render(frame, app, body),
    }
    render_footer(frame, app, footer);
    if app.help_open {
        help::render_overlay(frame, app.screen, area);
    }
    if !app.dialogs.is_empty() {
        // Dim what is behind, then draw the stack bottom to top.
        frame.buffer_mut().set_style(area, theme::BEHIND_DIALOG);
        for dialog in &app.dialogs {
            app.hits.dialog = dialog.render(frame, area);
        }
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) -> Vec<(Rect, Screen)> {
    let mut spans = vec![Span::styled(" rustdm ", theme::BRAND), Span::raw("  ")];
    let mut hits = Vec::new();
    let mut x = area.x + 10;
    for (i, screen) in Screen::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEPARATOR, theme::DIM));
            x += SEPARATOR.chars().count() as u16;
        }
        let style = if screen == app.screen {
            theme::CURRENT
        } else {
            theme::MUTED
        };
        let width = screen.title().len() as u16;
        if x + width <= area.right() {
            hits.push((Rect::new(x, area.y, width, 1), screen));
        }
        spans.push(Span::styled(screen.title(), style));
        x += width;
    }
    spans.push(Span::raw("   "));
    spans.extend(daemon_status(app));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    hits
}

fn daemon_status(app: &App) -> Vec<Span<'static>> {
    if !app.connected {
        return vec![Span::styled("rdmd reconnecting…", theme::ERROR)];
    }
    let limits = match &app.limits {
        Some(l) if l.max_bytes_per_sec > 0 => format!(
            "{} at once · speed cap {}",
            l.max_active,
            format_speed(l.max_bytes_per_sec)
        ),
        Some(l) => format!("{} at once · no speed cap", l.max_active),
        None => String::new(),
    };
    let total_speed: u64 = app.downloads.iter().map(|d| d.speed_bps).sum();
    vec![
        Span::styled(format!("rdmd connected  {limits}"), theme::MUTED),
        Span::raw("  "),
        Span::styled(
            format!("total {}", format_speed(total_speed)),
            theme::ACCENT,
        ),
    ]
}

/// The footer: a prompt while one is open, else the latest notice, else the
/// keys for the current screen that fit.
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = match app.screen {
        Screen::Downloads => downloads::prompt_line(app),
        _ => None,
    };
    let line = match (prompt, &app.notice) {
        (Some(line), _) => line,
        (None, Some(notice)) => {
            let style = if notice.is_error {
                theme::ERROR
            } else {
                theme::SUCCESS
            };
            Line::from(Span::styled(notice.text.clone(), style))
        }
        (None, None) => Line::from(Span::styled(
            keys_that_fit(help::keys(app.screen), area.width as usize),
            theme::DIM,
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// "a Add · p Pause · …", as many entries as fit in `width` columns.
fn keys_that_fit(keys: &[(&str, &str)], width: usize) -> String {
    let mut text = String::new();
    for (key, does) in keys {
        let entry = format!("{key} {does}");
        let extra = if text.is_empty() {
            0
        } else {
            SEPARATOR.chars().count()
        };
        if text.chars().count() + extra + entry.chars().count() > width {
            break;
        }
        if !text.is_empty() {
            text.push_str(SEPARATOR);
        }
        text.push_str(&entry);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::action::Action;
    use crate::tui::dialogs::add::{AddDialog, AddField};
    use crate::tui::dialogs::{Dialog, DialogTarget};
    use crate::tui::state::test_support::{app_with, sample_app, screen_text};
    use crate::tui::state::{App, Screen};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        screen_text(&terminal)
    }

    #[test]
    fn esc_on_downloads_does_not_quit() {
        let mut app = app_with(&[DownloadStatus::Active]);
        assert_eq!(handle_event(&mut app, &key(KeyCode::Esc)), None);
        assert_eq!(app.screen, Screen::Downloads);
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn s_opens_settings_and_esc_returns() {
        let mut app = App::default();
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('s'))), None);
        assert_eq!(app.screen, Screen::Settings);
        assert_eq!(handle_event(&mut app, &key(KeyCode::Esc)), None);
        assert_eq!(app.screen, Screen::Downloads);
    }

    #[test]
    fn l_opens_logs() {
        let mut app = App::default();
        handle_event(&mut app, &key(KeyCode::Char('l')));
        assert_eq!(app.screen, Screen::Logs);
    }

    #[test]
    fn question_mark_toggles_help_and_esc_closes_it() {
        let mut app = App::default();
        handle_event(&mut app, &key(KeyCode::Char('s')));
        handle_event(&mut app, &key(KeyCode::Char('?')));
        assert!(app.help_open);
        handle_event(&mut app, &key(KeyCode::Char('?')));
        assert!(!app.help_open);

        handle_event(&mut app, &key(KeyCode::Char('?')));
        handle_event(&mut app, &key(KeyCode::Esc));
        assert!(!app.help_open);
        assert_eq!(app.screen, Screen::Settings, "Esc only closed Help");
        assert!(draw(&mut app, 120, 40).contains("Settings"));
    }

    #[test]
    fn the_help_overlay_lists_the_keys_of_the_current_screen() {
        let mut app = sample_app();
        app.help_open = true;
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("Help"), "{text}");
        for (key, does) in help::keys(Screen::Downloads) {
            assert!(text.contains(does), "missing {key} {does}:\n{text}");
        }
    }

    #[test]
    fn clicking_a_screen_name_switches_screen() {
        let mut app = sample_app();
        draw(&mut app, 120, 40);
        let (logs, _) = *app
            .hits
            .screens
            .iter()
            .find(|(_, s)| *s == Screen::Logs)
            .expect("Logs is clickable");
        handle_event(&mut app, &click(logs.x, logs.y));
        assert_eq!(app.screen, Screen::Logs);
    }

    #[test]
    fn letters_typed_into_the_dialog_are_not_shortcuts() {
        let mut app = app_with(&[DownloadStatus::Active]);
        handle_event(&mut app, &key(KeyCode::Char('a')));
        for c in "qsl?pc".chars() {
            assert_eq!(handle_event(&mut app, &key(KeyCode::Char(c))), None);
        }
        assert_eq!(app.screen, Screen::Downloads);
        assert!(!app.help_open);
        let Some(Dialog::Add(add)) = app.dialogs.last() else {
            panic!("the dialog closed");
        };
        assert_eq!(add.field_text(AddField::Url), "qsl?pc");
        handle_event(&mut app, &key(KeyCode::Esc));
        assert!(app.dialogs.is_empty(), "Esc closes the dialog");
        assert_eq!(app.screen, Screen::Downloads);
    }

    #[test]
    fn a_paste_and_enter_submit_the_add_dialog() {
        let mut app = App::default();
        handle_event(&mut app, &key(KeyCode::Char('a')));
        handle_event(&mut app, &Event::Paste("https://example.com/a".into()));
        let Some(Action::Add(params)) = handle_event(&mut app, &key(KeyCode::Enter)) else {
            panic!("Enter did not submit");
        };
        assert_eq!(params[0].url, "https://example.com/a");
        assert_eq!(app.dialogs.len(), 1, "open until the daemon answers");
    }

    #[test]
    fn clicking_the_add_button_of_the_dialog_submits() {
        let mut app = App::default();
        app.dialogs.push(Dialog::Add(AddDialog::new(
            None,
            Some("https://example.com/a"),
        )));
        draw(&mut app, 100, 30);
        let (button, _) = *app
            .hits
            .dialog
            .iter()
            .find(|(_, t)| *t == DialogTarget::Key(KeyCode::Enter))
            .expect("Add button");
        let action = handle_event(&mut app, &click(button.x, button.y));
        assert!(matches!(action, Some(Action::Add(_))), "{action:?}");
    }

    #[test]
    fn the_footer_lists_keys_for_the_current_screen() {
        let mut app = sample_app();
        let footer = |text: String| text.lines().last().unwrap().to_string();
        assert!(footer(draw(&mut app, 120, 40)).contains("s Settings"));
        app.screen = Screen::Settings;
        assert!(footer(draw(&mut app, 120, 40)).contains("Esc Back"));
    }

    #[test]
    fn the_header_says_reconnecting_while_rdmd_is_away() {
        let mut app = sample_app();
        app.connected = false;
        let text = draw(&mut app, 120, 40);
        let header = text.lines().next().unwrap();
        assert!(header.contains("rdmd reconnecting…"), "{header}");
        assert!(header.contains("Downloads · Settings · Logs"), "{header}");
    }
}
