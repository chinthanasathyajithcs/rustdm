//! The Downloads screen: the table of downloads, its toolbar and tabs.

use crate::ipc::protocol::QueuePosition;
use crate::tui::action::Action;
use crate::tui::screens::hit;
use crate::tui::state::{App, Button, Mode, Tab};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;

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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DownloadStatus;
    use crate::tui::state::test_support::app_with;
    use crate::tui::state::{App, HitMap};
    use ratatui::layout::Rect;

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
}
