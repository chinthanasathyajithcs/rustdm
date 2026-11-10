//! The Downloads screen: the table of downloads, its toolbar and tabs.

use crate::ipc::protocol::QueuePosition;
use crate::tui::action::Action;
use crate::tui::format::format_speed;
use crate::tui::screens::hit;
use crate::tui::state::{App, Button, HitMap, Mode, Tab};
use crate::tui::theme;
use crate::tui::widgets::inspector::render_inspector;
use crate::tui::widgets::table::render_table;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Rows moved by Page Up / Page Down.
const PAGE_ROWS: isize = 10;

/// Turn a terminal event into an action for the daemon, updating local state on the way.
pub fn handle_event(app: &mut App, event: &Event) -> Option<Action> {
    match event {
        // Windows also reports key releases; act on presses only.
        Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
        Event::Mouse(mouse) => handle_mouse(app, mouse),
        Event::Paste(text) => {
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            match &mut app.mode {
                Mode::AddUrl(input) => input.push_str(&text),
                Mode::Browse | Mode::ConfirmCancel(_) => {
                    app.mode = Mode::AddUrl(text.trim().to_string())
                }
            }
            None
        }
        _ => None,
    }
}

fn handle_key(app: &mut App, key: &KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Action::Quit);
    }

    if let Mode::ConfirmCancel(id) = &app.mode {
        let id = id.clone();
        app.mode = Mode::Browse;
        return (key.code == KeyCode::Char('y')).then_some(Action::Cancel(id));
    }

    if let Mode::AddUrl(input) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => input.push(c),
            KeyCode::Enter => {
                let url = input.trim().to_string();
                app.mode = Mode::Browse;
                if !url.is_empty() {
                    return Some(Action::Add(url));
                }
            }
            _ => {}
        }
        return None;
    }

    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Up if shift => move_in_queue(app, QueuePosition::Up),
        KeyCode::Down if shift => move_in_queue(app, QueuePosition::Down),
        KeyCode::Char('K') => move_in_queue(app, QueuePosition::Up),
        KeyCode::Char('J') => move_in_queue(app, QueuePosition::Down),
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_selection(-1);
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_selection(1);
            None
        }
        KeyCode::PageUp => {
            app.move_selection(-PAGE_ROWS);
            None
        }
        KeyCode::PageDown => {
            app.move_selection(PAGE_ROWS);
            None
        }
        KeyCode::Home => {
            app.move_selection(isize::MIN / 2);
            None
        }
        KeyCode::End => {
            app.move_selection(isize::MAX / 2);
            None
        }
        KeyCode::Tab | KeyCode::Right => {
            app.select_tab(app.tab.offset(1));
            None
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.select_tab(app.tab.offset(-1));
            None
        }
        KeyCode::Char(c @ '1'..='5') => {
            app.select_tab(Tab::ALL[c as usize - '1' as usize]);
            None
        }
        KeyCode::Char('a') => press(app, Button::Add),
        KeyCode::Char('p') => press(app, Button::Pause),
        KeyCode::Char('r') => press(app, Button::Resume),
        KeyCode::Char('c') => press(app, Button::Cancel),
        KeyCode::Char('x') | KeyCode::Delete => press(app, Button::Remove),
        _ => None,
    }
}

fn handle_mouse(app: &mut App, mouse: &MouseEvent) -> Option<Action> {
    match app.mode {
        Mode::AddUrl(_) => return None,
        Mode::ConfirmCancel(_) => {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                app.mode = Mode::Browse;
            }
            return None;
        }
        Mode::Browse => {}
    }
    let at = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(button) = hit(&app.hits.buttons, at) {
                return press(app, button);
            }
            if let Some(tab) = hit(&app.hits.tabs, at) {
                app.select_tab(tab);
            } else if let Some(id) = hit(&app.hits.rows, at) {
                app.selected = Some(id);
            }
            None
        }
        MouseEventKind::ScrollDown => {
            app.move_selection(1);
            None
        }
        MouseEventKind::ScrollUp => {
            app.move_selection(-1);
            None
        }
        _ => None,
    }
}

fn move_in_queue(app: &mut App, position: QueuePosition) -> Option<Action> {
    match app.selected_summary() {
        Some(d) => Some(Action::Move(d.id.clone(), position)),
        None => {
            app.notify("Select a download first", true);
            None
        }
    }
}

/// Run a toolbar button. Buttons that act on a download need one selected.
fn press(app: &mut App, button: Button) -> Option<Action> {
    let make: fn(String) -> Action = match button {
        Button::Add => {
            app.mode = Mode::AddUrl(String::new());
            return None;
        }
        Button::Quit => return Some(Action::Quit),
        Button::Cancel => {
            // Cancelling stops the download for good unless resumed, so ask first.
            match app.selected_summary() {
                Some(d) => app.mode = Mode::ConfirmCancel(d.id.clone()),
                None => app.notify("Select a download first", true),
            }
            return None;
        }
        Button::Pause => Action::Pause,
        Button::Resume => Action::Resume,
        Button::Remove => Action::Remove,
    };
    match app.selected_summary() {
        Some(d) => Some(make(d.id.clone())),
        None => {
            app.notify("Select a download first", true);
            None
        }
    }
}
/// Height of the details pane when the window is tall enough.
const INSPECTOR_HEIGHT: u16 = 16;

/// Smallest table height (borders, header and two rows) kept before the details pane shrinks.
const MIN_TABLE_HEIGHT: u16 = 5;

/// Positions of the toolbar buttons on `area`'s first line, left to right.
pub fn toolbar_layout(area: Rect) -> Vec<(Rect, Button)> {
    let mut x = area.x;
    let mut buttons = Vec::new();
    for button in Button::ALL {
        let width = button.label().len() as u16 + 4;
        if x + width > area.right() {
            break;
        }
        buttons.push((Rect::new(x, area.y, width, 1), button));
        x += width + 1;
    }
    buttons
}

fn tab_label(app: &App, tab: Tab) -> String {
    format!(" {} ({}) ", tab.title(), app.count(tab))
}

/// Positions of the tabs on `area`'s first line, left to right.
pub fn tabs_layout(app: &App, area: Rect) -> Vec<(Rect, Tab)> {
    let mut x = area.x;
    let mut tabs = Vec::new();
    for tab in Tab::ALL {
        let width = tab_label(app, tab).len() as u16;
        if x + width > area.right() {
            break;
        }
        tabs.push((Rect::new(x, area.y, width, 1), tab));
        x += width + 1;
    }
    tabs
}

/// Draw the whole dashboard and record what is clickable in `app.hits`.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let inspector_height = INSPECTOR_HEIGHT.min(area.height.saturating_sub(4 + MIN_TABLE_HEIGHT));
    let [header, toolbar, tabs, table, inspector, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(MIN_TABLE_HEIGHT),
        Constraint::Length(inspector_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let mut hits = HitMap::default();
    render_header(frame, app, header);
    hits.buttons = render_toolbar(frame, toolbar);
    hits.tabs = render_tabs(frame, app, tabs);
    hits.rows = render_table(frame, app, table);
    render_inspector(frame, app, inspector);
    render_footer(frame, app, footer);
    app.hits = hits;
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let status = if !app.connected {
        Span::styled("rdmd not connected", theme::ERROR)
    } else {
        let limits = match &app.limits {
            Some(l) if l.max_bytes_per_sec > 0 => format!(
                "{} at once · speed cap {}",
                l.max_active,
                format_speed(l.max_bytes_per_sec)
            ),
            Some(l) => format!("{} at once · no speed cap", l.max_active),
            None => String::new(),
        };
        Span::styled(format!("rdmd connected  {limits}"), theme::MUTED)
    };
    let total_speed: u64 = app.downloads.iter().map(|d| d.speed_bps).sum();
    let line = Line::from(vec![
        Span::styled(" rustdm ", theme::BRAND),
        Span::raw("  "),
        status,
        Span::raw("  "),
        Span::styled(
            format!("total {}", format_speed(total_speed)),
            theme::ACCENT,
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_toolbar(frame: &mut Frame, area: Rect) -> Vec<(Rect, Button)> {
    let buttons = toolbar_layout(area);
    for (rect, button) in &buttons {
        let style = match button {
            Button::Add => theme::BUTTON_ADD,
            Button::Quit => theme::BUTTON_QUIT,
            _ => theme::BUTTON,
        };
        let text = Paragraph::new(format!("[ {} ]", button.label())).style(style);
        frame.render_widget(text, *rect);
    }
    buttons
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) -> Vec<(Rect, Tab)> {
    let tabs = tabs_layout(app, area);
    for (rect, tab) in &tabs {
        let style = if *tab == app.tab {
            theme::BRAND
        } else {
            theme::MUTED
        };
        frame.render_widget(Paragraph::new(tab_label(app, *tab)).style(style), *rect);
    }
    tabs
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match (&app.mode, &app.notice) {
        (Mode::AddUrl(input), _) => {
            let prompt = "Add URL: ";
            let cursor_x = area.x + (prompt.len() + input.chars().count()) as u16;
            frame.set_cursor_position(Position::new(
                cursor_x.min(area.right().saturating_sub(1)),
                area.y,
            ));
            Line::from(vec![
                Span::styled(
                    prompt,
                    theme::PROMPT,
                ),
                Span::raw(input.clone()),
                Span::styled(
                    "   Enter adds · Esc cancels",
                    theme::DIM,
                ),
            ])
        }
        (Mode::ConfirmCancel(id), _) => {
            let name = app
                .downloads
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.filename.as_str())
                .unwrap_or("");
            Line::from(vec![
                Span::styled(
                    format!("Cancel {id} ({name})? "),
                    theme::CONFIRM,
                ),
                Span::styled(
                    "y cancels · any other key keeps it",
                    theme::DIM,
                ),
            ])
        }
        (Mode::Browse, Some(notice)) => {
            let style = if notice.is_error {
                theme::ERROR
            } else {
                theme::SUCCESS
            };
            Line::from(Span::styled(notice.text.clone(), style))
        }
        (Mode::Browse, None) => Line::from(Span::styled(
            "a add · p pause · r resume · c cancel · x remove · ←/→ tabs · ↑/↓ select · ⇧↑/⇧↓ reorder · q quit",
            theme::DIM,
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::ipc::protocol::{DownloadSummary, SegmentProgress, TelemetryTick};
    use crate::tui::state::test_support::app_with;
    use crate::tui::state::{App, HitMap};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use std::path::PathBuf;

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

    #[test]
    fn shifted_arrows_and_capital_j_k_move_the_selected_download_in_the_queue() {
        use DownloadStatus::*;
        let mut app = app_with(&[Queued, Queued]);
        let shift = |code| Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT));
        assert_eq!(
            handle_event(&mut app, &shift(KeyCode::Down)),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Down))
        );
        assert_eq!(
            handle_event(&mut app, &shift(KeyCode::Up)),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Up))
        );
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('J'))),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Down))
        );
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('K'))),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Up))
        );
        // Plain arrows still only move the highlight.
        assert_eq!(handle_event(&mut app, &key(KeyCode::Down)), None);
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
    }

    #[test]
    fn tabs_filter_by_status_and_keep_a_valid_selection() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Queued, Completed, Paused, Failed]);
        assert_eq!(app.count(Tab::Stopped), 2);

        handle_event(&mut app, &key(KeyCode::Char('5')));
        assert_eq!(app.tab, Tab::Stopped);
        assert_eq!(app.selected.as_deref(), Some("dl_4"));

        handle_event(&mut app, &key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::All, "tabs wrap around");
    }

    #[test]
    fn arrow_keys_move_the_selection_and_stop_at_the_ends() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Queued, Paused]);
        handle_event(&mut app, &key(KeyCode::Down));
        handle_event(&mut app, &key(KeyCode::Down));
        handle_event(&mut app, &key(KeyCode::Down));
        assert_eq!(app.selected.as_deref(), Some("dl_3"));
        handle_event(&mut app, &key(KeyCode::Home));
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
    }

    #[test]
    fn action_keys_target_the_selected_download() {
        let mut app = app_with(&[DownloadStatus::Active]);
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('p'))),
            Some(Action::Pause("dl_1".to_string()))
        );
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Delete)),
            Some(Action::Remove("dl_1".to_string()))
        );
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn actions_without_a_selection_explain_instead_of_failing() {
        let mut app = App::default();
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('p'))), None);
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn typing_or_pasting_a_url_then_enter_adds_it() {
        let mut app = App::default();
        handle_event(&mut app, &key(KeyCode::Char('a')));
        for c in "http://x/a".chars() {
            handle_event(&mut app, &key(KeyCode::Char(c)));
        }
        handle_event(&mut app, &key(KeyCode::Backspace));
        handle_event(&mut app, &Event::Paste("b.iso\n".to_string()));
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Enter)),
            Some(Action::Add("http://x/b.iso".to_string()))
        );
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn letters_typed_into_the_url_box_are_not_shortcuts() {
        let mut app = app_with(&[DownloadStatus::Active]);
        handle_event(&mut app, &key(KeyCode::Char('a')));
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('q'))), None);
        assert_eq!(app.mode, Mode::AddUrl("q".to_string()));
        handle_event(&mut app, &key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn pasting_while_browsing_opens_the_add_box() {
        let mut app = App::default();
        handle_event(&mut app, &Event::Paste("  http://x/y  ".to_string()));
        assert_eq!(app.mode, Mode::AddUrl("http://x/y".to_string()));
    }

    #[test]
    fn clicks_hit_buttons_tabs_and_rows() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Queued]);
        app.hits = HitMap {
            buttons: vec![(Rect::new(0, 1, 8, 1), Button::Cancel)],
            tabs: vec![(Rect::new(10, 2, 8, 1), Tab::Queued)],
            rows: vec![
                (Rect::new(0, 5, 80, 1), "dl_1".to_string()),
                (Rect::new(0, 6, 80, 1), "dl_2".to_string()),
            ],
        };

        handle_event(&mut app, &click(3, 6));
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
        // The Cancel button asks first; "y" confirms.
        assert_eq!(handle_event(&mut app, &click(2, 1)), None);
        assert_eq!(app.mode, Mode::ConfirmCancel("dl_2".to_string()));
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('y'))),
            Some(Action::Cancel("dl_2".to_string()))
        );
        handle_event(&mut app, &click(12, 2));
        assert_eq!(app.tab, Tab::Queued);
        assert_eq!(
            handle_event(&mut app, &click(70, 30)),
            None,
            "empty space does nothing"
        );
    }

    #[test]
    fn mouse_wheel_moves_the_selection() {
        let mut app = app_with(&[DownloadStatus::Active, DownloadStatus::Queued]);
        let scroll = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        handle_event(&mut app, &scroll(MouseEventKind::ScrollDown));
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
        handle_event(&mut app, &scroll(MouseEventKind::ScrollUp));
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
    }

    #[test]
    fn cancel_asks_first_and_only_y_confirms() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active]);
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('c'))), None);
        assert_eq!(app.mode, Mode::ConfirmCancel("dl_1".to_string()));
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('y'))),
            Some(Action::Cancel("dl_1".to_string()))
        );
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn any_other_key_or_a_click_backs_out_of_cancelling() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active]);
        handle_event(&mut app, &key(KeyCode::Char('c')));
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('n'))), None);
        assert_eq!(app.mode, Mode::Browse);

        handle_event(&mut app, &key(KeyCode::Char('c')));
        assert_eq!(handle_event(&mut app, &click(0, 0)), None);
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn toolbar_buttons_sit_side_by_side_and_stop_at_the_edge() {
        let buttons = toolbar_layout(Rect::new(0, 1, 200, 1));
        assert_eq!(buttons.len(), Button::ALL.len());
        for pair in buttons.windows(2) {
            assert!(pair[0].0.right() < pair[1].0.x, "buttons must not touch");
        }
        let narrow = toolbar_layout(Rect::new(0, 1, 20, 1));
        assert!(narrow.iter().all(|(r, _)| r.right() <= 20));
    }

    fn sample_app() -> App {
        let mut app = App {
            connected: true,
            ..App::default()
        };
        app.set_downloads(vec![DownloadSummary {
            id: "dl_1".to_string(),
            url: "http://example.com/ubuntu.iso".to_string(),
            destination: PathBuf::from("/downloads/ubuntu.iso"),
            filename: "ubuntu.iso".to_string(),
            status: DownloadStatus::Active,
            downloaded_bytes: 40 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            eta_secs: Some(30),
            error_message: None,
            max_bytes_per_sec: 0,
        }]);
        app.apply_tick(TelemetryTick {
            id: "dl_1".to_string(),
            downloaded_bytes: 40 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            active_connections: 2,
            segments: vec![SegmentProgress {
                start: 0,
                end: 40 * 1024 * 1024,
                downloaded_bytes: 40 * 1024 * 1024,
                state: "completed".to_string(),
            }],
            lanes: vec![
                SegmentProgress {
                    start: 0,
                    end: 50 * 1024 * 1024,
                    downloaded_bytes: 40 * 1024 * 1024,
                    state: "active".to_string(),
                },
                SegmentProgress {
                    start: 50 * 1024 * 1024,
                    end: 100 * 1024 * 1024,
                    downloaded_bytes: 0,
                    state: "active".to_string(),
                },
            ],
        });
        app
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn dashboard_shows_table_details_and_records_click_targets() {
        let mut app = sample_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);

        for expected in [
            "rustdm",
            "[ + Add ]",
            "[ || Pause ]",
            "Downloading (1)",
            "ubuntu.iso",
            "40.0 MiB / 100.0 MiB",
            "Segments",
            "Connections (2 active)",
            "Speed",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert_eq!(app.hits.buttons.len(), Button::ALL.len());
        assert_eq!(app.hits.tabs.len(), Tab::ALL.len());
        assert_eq!(app.hits.rows.len(), 1);
        assert_eq!(app.hits.rows[0].1, "dl_1");
    }

    #[test]
    fn small_windows_still_render_without_panicking() {
        let mut app = sample_app();
        for (w, h) in [(20, 5), (40, 10), (80, 12), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn add_mode_shows_the_url_being_typed() {
        let mut app = sample_app();
        app.mode = Mode::AddUrl("http://example.com/a".to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("Add URL: http://example.com/a"));
    }

    #[test]
    fn cancelling_asks_in_the_footer() {
        let mut app = sample_app();
        let id = app.downloads[0].id.clone();
        let name = app.downloads[0].filename.clone();
        app.mode = Mode::ConfirmCancel(id.clone());
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains(&format!("Cancel {id} ({name})?")), "{text}");
        assert!(text.contains("y cancels"), "{text}");
    }
}
