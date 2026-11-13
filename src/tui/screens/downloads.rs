//! The Downloads screen: the table of downloads, its toolbar and tabs.

use crate::ipc::protocol::QueuePosition;
use crate::tui::action::Action;
use crate::tui::dialogs::add::AddDialog;
use crate::tui::dialogs::confirm::ConfirmDialog;
use crate::tui::dialogs::speed::SpeedDialog;
use crate::tui::dialogs::Dialog;
use crate::tui::screens::hit;
use crate::tui::state::{App, Button, Tab};
use crate::tui::theme;
use crate::tui::widgets::inspector::render_inspector;
use crate::tui::widgets::table::render_table;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
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
            // A paste while browsing is almost always a link to add.
            open_add_dialog(app, Some(text));
            None
        }
        _ => None,
    }
}

fn handle_key(app: &mut App, key: &KeyEvent) -> Option<Action> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Up if shift => move_in_queue(app, QueuePosition::Up),
        KeyCode::Down if shift => move_in_queue(app, QueuePosition::Down),
        KeyCode::Char('K') => move_in_queue(app, QueuePosition::Up),
        KeyCode::Char('J') => move_in_queue(app, QueuePosition::Down),
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
        KeyCode::Char('v') => press(app, Button::Speed),
        KeyCode::Char('o') => open_selected_folder(app),
        _ => None,
    }
}

fn handle_mouse(app: &mut App, mouse: &MouseEvent) -> Option<Action> {
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

/// The selected download's file: `destination` is the file once the download
/// has started (it ends in the file name), and the folder it goes in before
/// that, which may not exist yet.
fn open_selected_folder(app: &mut App) -> Option<Action> {
    let Some(d) = app.selected_summary() else {
        app.notify("Select a download first", true);
        return None;
    };
    let is_file = d.destination.file_name() == Some(std::ffi::OsStr::new(&d.filename));
    let path = if is_file {
        d.destination.clone()
    } else {
        d.destination.join(&d.filename)
    };
    Some(Action::OpenFolder(path))
}

fn open_add_dialog(app: &mut App, paste: Option<&str>) {
    let dialog = AddDialog::new(app.defaults.as_ref(), paste);
    app.dialogs.push(Dialog::Add(Box::new(dialog)));
}

/// Run a toolbar button. Buttons that act on a download need one selected.
fn press(app: &mut App, button: Button) -> Option<Action> {
    match button {
        Button::Add => {
            open_add_dialog(app, None);
            return None;
        }
        Button::Quit => return Some(Action::Quit),
        _ => {}
    }
    let Some(d) = app.selected_summary().cloned() else {
        app.notify("Select a download first", true);
        return None;
    };
    match button {
        Button::Pause => Some(Action::Pause(d.id)),
        Button::Resume => Some(Action::Resume(d.id)),
        Button::Remove => Some(Action::Remove(d.id)),
        Button::Cancel => {
            // Cancelling stops the download for good unless resumed, so ask first.
            app.dialogs.push(Dialog::Confirm(ConfirmDialog::cancel(&d)));
            None
        }
        Button::Speed => {
            app.dialogs.push(Dialog::Speed(SpeedDialog::new(&d)));
            None
        }
        Button::Add | Button::Quit => None,
    }
}
/// Terminal rows needed before the details pane shows; below this the table gets them all.
pub const INSPECTOR_MIN_ROWS: u16 = 24;

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

/// Draw the Downloads screen into `area` and record its click targets.
pub fn render(frame: &mut Frame, app: &mut App, area: Rect) {
    let inspector_height = if frame.area().height < INSPECTOR_MIN_ROWS {
        0
    } else {
        INSPECTOR_HEIGHT.min(area.height.saturating_sub(2 + MIN_TABLE_HEIGHT))
    };
    let [toolbar, tabs, table, inspector] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(MIN_TABLE_HEIGHT),
        Constraint::Length(inspector_height),
    ])
    .areas(area);

    app.hits.buttons = render_toolbar(frame, toolbar);
    app.hits.tabs = render_tabs(frame, app, tabs);
    app.hits.rows = render_table(frame, app, table);
    render_inspector(frame, app, inspector);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::dialogs::add::AddField;
    use crate::tui::state::test_support::{app_with, sample_app, screen_text};
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
    }

    #[test]
    fn actions_without_a_selection_explain_instead_of_failing() {
        let mut app = App::default();
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('p'))), None);
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn o_targets_the_selected_download_path() {
        let mut app = app_with(&[DownloadStatus::Completed]);
        let mut finished = app.downloads[0].clone();
        finished.destination = PathBuf::from("/downloads/dl_1.bin");
        app.set_downloads(vec![finished]);
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('o'))),
            Some(Action::OpenFolder(PathBuf::from("/downloads/dl_1.bin"))),
            "a started download reports its file path"
        );

        let folder = tempfile::tempdir().unwrap();
        let mut queued = app.downloads[0].clone();
        queued.destination = folder.path().to_path_buf();
        app.set_downloads(vec![queued]);
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('o'))),
            Some(Action::OpenFolder(folder.path().join("dl_1.bin"))),
            "before it starts, the destination is its folder"
        );

        let mut waiting = app.downloads[0].clone();
        waiting.destination = folder.path().join("not made yet");
        app.set_downloads(vec![waiting]);
        assert_eq!(
            handle_event(&mut app, &key(KeyCode::Char('o'))),
            Some(Action::OpenFolder(
                folder.path().join("not made yet").join("dl_1.bin")
            )),
            "a folder that doesn't exist yet is still the folder, not the file"
        );
    }

    #[test]
    fn o_without_a_selection_asks_for_one() {
        let mut app = App::default();
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('o'))), None);
        assert_eq!(
            app.notice.as_ref().map(|n| n.text.as_str()),
            Some("Select a download first")
        );
    }

    fn add_dialog_urls(app: &App) -> Vec<String> {
        match app.dialogs.last() {
            Some(Dialog::Add(add)) => add.urls(),
            other => panic!("expected the Add dialog, got {other:?}"),
        }
    }

    #[test]
    fn a_opens_the_add_dialog_with_the_daemon_defaults() {
        let mut app = App {
            defaults: Some(crate::ipc::protocol::DownloadDefaults {
                folder: "/dl".into(),
                connections: 3,
                max_bytes_per_sec: 0,
            }),
            ..App::default()
        };
        assert_eq!(handle_event(&mut app, &key(KeyCode::Char('a'))), None);
        let Some(Dialog::Add(add)) = app.dialogs.last() else {
            panic!("no Add dialog");
        };
        assert_eq!(add.field_text(AddField::Folder), "/dl");
        assert_eq!(add.field_text(AddField::Connections), "3");
    }

    #[test]
    fn pasting_while_browsing_opens_the_add_dialog_with_the_paste() {
        let mut app = App::default();
        handle_event(&mut app, &Event::Paste("  http://x/y  \r\n".to_string()));
        assert_eq!(add_dialog_urls(&app), vec!["http://x/y"]);
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
            ..HitMap::default()
        };

        handle_event(&mut app, &click(3, 6));
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
        // The Cancel button asks first.
        assert_eq!(handle_event(&mut app, &click(2, 1)), None);
        let Some(Dialog::Confirm(question)) = app.dialogs.last() else {
            panic!("Cancel did not ask");
        };
        assert_eq!(question.question(), "Cancel dl_2 (dl_2.bin)?");
        app.dialogs.clear();
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
    fn toolbar_buttons_sit_side_by_side_and_stop_at_the_edge() {
        let buttons = toolbar_layout(Rect::new(0, 1, 200, 1));
        assert_eq!(buttons.len(), Button::ALL.len());
        for pair in buttons.windows(2) {
            assert!(pair[0].0.right() < pair[1].0.x, "buttons must not touch");
        }
        let narrow = toolbar_layout(Rect::new(0, 1, 20, 1));
        assert!(narrow.iter().all(|(r, _)| r.right() <= 20));
    }

    #[test]
    fn dashboard_shows_table_details_and_records_click_targets() {
        let mut app = sample_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
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
    fn the_inspector_hides_below_24_rows() {
        let mut app = sample_app();
        let draw = |app: &mut App, height: u16| {
            let mut terminal = Terminal::new(TestBackend::new(120, height)).unwrap();
            terminal
                .draw(|f| crate::tui::screens::render(f, app))
                .unwrap();
            screen_text(&terminal)
        };
        assert!(draw(&mut app, INSPECTOR_MIN_ROWS).contains("Segments"));
        let short = draw(&mut app, INSPECTOR_MIN_ROWS - 1);
        assert!(!short.contains("Segments"), "{short}");
        assert!(short.contains("ubuntu.iso"), "the table keeps the room");
    }

    #[test]
    fn small_windows_still_render_without_panicking() {
        let mut app = sample_app();
        for (w, h) in [(20, 5), (40, 10), (80, 12), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| crate::tui::screens::render(f, &mut app))
                .unwrap();
        }
    }

    #[test]
    fn the_add_dialog_draws_over_the_screen() {
        let mut app = sample_app();
        app.dialogs.push(Dialog::Add(Box::new(AddDialog::new(
            None,
            Some("http://example.com/a"),
        ))));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Add a download"), "{text}");
        assert!(text.contains("http://example.com/a"), "{text}");
        assert!(!app.hits.dialog.is_empty());
    }
}
