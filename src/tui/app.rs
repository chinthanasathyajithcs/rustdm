use crate::ipc::protocol::{
    DaemonLimits, DownloadStatus, DownloadSummary, QueuePosition, TelemetryTick,
};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::{HashMap, VecDeque};

/// Speed samples kept per download for the speed graph: about 30 s at 4 ticks a second.
pub const SPEED_HISTORY_LEN: usize = 120;
/// Rows moved by Page Up / Page Down.
const PAGE_ROWS: isize = 10;

/// Status filters shown as tabs above the downloads table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    All,
    Active,
    Queued,
    Completed,
    Stopped,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::All,
        Tab::Active,
        Tab::Queued,
        Tab::Completed,
        Tab::Stopped,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Active => "Downloading",
            Tab::Queued => "Queued",
            Tab::Completed => "Completed",
            Tab::Stopped => "Stopped",
        }
    }

    pub fn includes(self, status: DownloadStatus) -> bool {
        match self {
            Tab::All => true,
            Tab::Active => matches!(
                status,
                DownloadStatus::Active | DownloadStatus::Pausing | DownloadStatus::Retrying
            ),
            Tab::Queued => status == DownloadStatus::Queued,
            Tab::Completed => status == DownloadStatus::Completed,
            Tab::Stopped => matches!(
                status,
                DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Cancelled
            ),
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn offset(self, delta: isize) -> Tab {
        let len = Tab::ALL.len() as isize;
        let next = (self.index() as isize + delta).rem_euclid(len);
        Tab::ALL[next as usize]
    }
}

/// Toolbar buttons, in the order they are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Add,
    Pause,
    Resume,
    Cancel,
    Remove,
    Quit,
}

impl Button {
    pub const ALL: [Button; 6] = [
        Button::Add,
        Button::Pause,
        Button::Resume,
        Button::Cancel,
        Button::Remove,
        Button::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Button::Add => "+ Add",
            Button::Pause => "|| Pause",
            Button::Resume => "> Resume",
            Button::Cancel => "x Cancel",
            Button::Remove => "- Remove",
            Button::Quit => "Quit",
        }
    }
}

/// Something the dashboard asks the daemon to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Add(String),
    Pause(String),
    Resume(String),
    Cancel(String),
    Remove(String),
    /// Move a download one place up or down the queue.
    Move(String, QueuePosition),
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Typing a URL to add; holds the text so far.
    AddUrl(String),
    /// Waiting for `y` to cancel this download.
    ConfirmCancel(String),
}

/// Screen areas the last frame drew, so a mouse click can find what it hit.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    pub buttons: Vec<(Rect, Button)>,
    pub tabs: Vec<(Rect, Tab)>,
    /// One entry per table row on screen, with that row's download id.
    pub rows: Vec<(Rect, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
}

pub struct App {
    /// Every download the daemon knows, in queue order.
    pub downloads: Vec<DownloadSummary>,
    /// Latest telemetry per download id.
    pub ticks: HashMap<String, TelemetryTick>,
    pub speed_history: HashMap<String, VecDeque<u64>>,
    pub limits: Option<DaemonLimits>,
    pub connected: bool,
    pub tab: Tab,
    pub selected: Option<String>,
    /// Index of the first visible table row.
    pub table_offset: usize,
    pub mode: Mode,
    pub notice: Option<Notice>,
    pub hits: HitMap,
}

impl Default for App {
    fn default() -> Self {
        Self {
            downloads: Vec::new(),
            ticks: HashMap::new(),
            speed_history: HashMap::new(),
            limits: None,
            connected: false,
            tab: Tab::All,
            selected: None,
            table_offset: 0,
            mode: Mode::Browse,
            notice: None,
            hits: HitMap::default(),
        }
    }
}

impl App {
    /// Downloads in the current tab, in queue order.
    pub fn visible(&self) -> Vec<&DownloadSummary> {
        self.downloads
            .iter()
            .filter(|d| self.tab.includes(d.status))
            .collect()
    }

    pub fn count(&self, tab: Tab) -> usize {
        self.downloads
            .iter()
            .filter(|d| tab.includes(d.status))
            .count()
    }

    pub fn selected_summary(&self) -> Option<&DownloadSummary> {
        let id = self.selected.as_ref()?;
        self.visible().into_iter().find(|d| &d.id == id)
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.visible().iter().position(|d| &d.id == id)
    }

    /// Replace the list with the daemon's, which comes in queue order.
    pub fn set_downloads(&mut self, downloads: Vec<DownloadSummary>) {
        self.ticks
            .retain(|id, _| downloads.iter().any(|d| &d.id == id));
        self.speed_history
            .retain(|id, _| downloads.iter().any(|d| &d.id == id));
        self.downloads = downloads;
        self.ensure_selection();
    }

    pub fn apply_tick(&mut self, tick: TelemetryTick) {
        let history = self.speed_history.entry(tick.id.clone()).or_default();
        if history.len() == SPEED_HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(tick.speed_bps);
        self.ticks.insert(tick.id.clone(), tick);
    }

    pub fn notify(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error,
        });
    }

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.table_offset = 0;
        self.ensure_selection();
    }

    /// Keep the selection on a download in the current tab, falling back to the first one.
    fn ensure_selection(&mut self) {
        if self.selected_index().is_none() {
            self.selected = self.visible().first().map(|d| d.id.clone());
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.selected = Some(visible[next].id.clone());
    }

    /// Turn a terminal event into an action for the daemon, updating local state on the way.
    pub fn handle_event(&mut self, event: &Event) -> Option<Action> {
        match event {
            // Windows also reports key releases; act on presses only.
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                let text: String = text.chars().filter(|c| !c.is_control()).collect();
                match &mut self.mode {
                    Mode::AddUrl(input) => input.push_str(&text),
                    Mode::Browse | Mode::ConfirmCancel(_) => {
                        self.mode = Mode::AddUrl(text.trim().to_string())
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, key: &KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }

        if let Mode::ConfirmCancel(id) = &self.mode {
            let id = id.clone();
            self.mode = Mode::Browse;
            return (key.code == KeyCode::Char('y')).then_some(Action::Cancel(id));
        }

        if let Mode::AddUrl(input) = &mut self.mode {
            match key.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Enter => {
                    let url = input.trim().to_string();
                    self.mode = Mode::Browse;
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
            KeyCode::Up if shift => self.move_in_queue(QueuePosition::Up),
            KeyCode::Down if shift => self.move_in_queue(QueuePosition::Down),
            KeyCode::Char('K') => self.move_in_queue(QueuePosition::Up),
            KeyCode::Char('J') => self.move_in_queue(QueuePosition::Down),
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                None
            }
            KeyCode::PageUp => {
                self.move_selection(-PAGE_ROWS);
                None
            }
            KeyCode::PageDown => {
                self.move_selection(PAGE_ROWS);
                None
            }
            KeyCode::Home => {
                self.move_selection(isize::MIN / 2);
                None
            }
            KeyCode::End => {
                self.move_selection(isize::MAX / 2);
                None
            }
            KeyCode::Tab | KeyCode::Right => {
                self.select_tab(self.tab.offset(1));
                None
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.select_tab(self.tab.offset(-1));
                None
            }
            KeyCode::Char(c @ '1'..='5') => {
                self.select_tab(Tab::ALL[c as usize - '1' as usize]);
                None
            }
            KeyCode::Char('a') => self.press(Button::Add),
            KeyCode::Char('p') => self.press(Button::Pause),
            KeyCode::Char('r') => self.press(Button::Resume),
            KeyCode::Char('c') => self.press(Button::Cancel),
            KeyCode::Char('x') | KeyCode::Delete => self.press(Button::Remove),
            _ => None,
        }
    }

    fn handle_mouse(&mut self, mouse: &MouseEvent) -> Option<Action> {
        match self.mode {
            Mode::AddUrl(_) => return None,
            Mode::ConfirmCancel(_) => {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.mode = Mode::Browse;
                }
                return None;
            }
            Mode::Browse => {}
        }
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(button) = hit(&self.hits.buttons, at) {
                    return self.press(button);
                }
                if let Some(tab) = hit(&self.hits.tabs, at) {
                    self.select_tab(tab);
                } else if let Some(id) = hit(&self.hits.rows, at) {
                    self.selected = Some(id);
                }
                None
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                None
            }
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                None
            }
            _ => None,
        }
    }

    fn move_in_queue(&mut self, position: QueuePosition) -> Option<Action> {
        match self.selected_summary() {
            Some(d) => Some(Action::Move(d.id.clone(), position)),
            None => {
                self.notify("Select a download first", true);
                None
            }
        }
    }

    /// Run a toolbar button. Buttons that act on a download need one selected.
    fn press(&mut self, button: Button) -> Option<Action> {
        let make: fn(String) -> Action = match button {
            Button::Add => {
                self.mode = Mode::AddUrl(String::new());
                return None;
            }
            Button::Quit => return Some(Action::Quit),
            Button::Cancel => {
                // Cancelling stops the download for good unless resumed, so ask first.
                match self.selected_summary() {
                    Some(d) => self.mode = Mode::ConfirmCancel(d.id.clone()),
                    None => self.notify("Select a download first", true),
                }
                return None;
            }
            Button::Pause => Action::Pause,
            Button::Resume => Action::Resume,
            Button::Remove => Action::Remove,
        };
        match self.selected_summary() {
            Some(d) => Some(make(d.id.clone())),
            None => {
                self.notify("Select a download first", true);
                None
            }
        }
    }
}

fn hit<T: Clone>(areas: &[(Rect, T)], at: Position) -> Option<T> {
    areas
        .iter()
        .find(|(rect, _)| rect.contains(at))
        .map(|(_, value)| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn summary(id: &str, status: DownloadStatus) -> DownloadSummary {
        DownloadSummary {
            id: id.to_string(),
            url: format!("http://example.com/{id}.bin"),
            destination: PathBuf::from("/downloads"),
            filename: format!("{id}.bin"),
            status,
            downloaded_bytes: 0,
            total_bytes: None,
            speed_bps: 0,
            eta_secs: None,
            error_message: None,
            max_bytes_per_sec: 0,
        }
    }

    fn app_with(statuses: &[DownloadStatus]) -> App {
        let mut app = App::default();
        let downloads = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| summary(&format!("dl_{}", i + 1), *s))
            .collect();
        app.set_downloads(downloads);
        app
    }

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
    fn downloads_keep_the_daemon_queue_order_and_the_first_is_selected() {
        let mut app = App::default();
        app.set_downloads(vec![
            summary("dl_10", DownloadStatus::Queued),
            summary("dl_2", DownloadStatus::Active),
        ]);
        let ids: Vec<_> = app.downloads.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["dl_10", "dl_2"]);
        assert_eq!(app.selected.as_deref(), Some("dl_10"));
    }

    #[test]
    fn shifted_arrows_and_capital_j_k_move_the_selected_download_in_the_queue() {
        use DownloadStatus::*;
        let mut app = app_with(&[Queued, Queued]);
        let shift = |code| Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT));
        assert_eq!(
            app.handle_event(&shift(KeyCode::Down)),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Down))
        );
        assert_eq!(
            app.handle_event(&shift(KeyCode::Up)),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Up))
        );
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('J'))),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Down))
        );
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('K'))),
            Some(Action::Move("dl_1".to_string(), QueuePosition::Up))
        );
        // Plain arrows still only move the highlight.
        assert_eq!(app.handle_event(&key(KeyCode::Down)), None);
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
    }

    #[test]
    fn tabs_filter_by_status_and_keep_a_valid_selection() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Queued, Completed, Paused, Failed]);
        assert_eq!(app.count(Tab::Stopped), 2);

        app.handle_event(&key(KeyCode::Char('5')));
        assert_eq!(app.tab, Tab::Stopped);
        assert_eq!(app.selected.as_deref(), Some("dl_4"));

        app.handle_event(&key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::All, "tabs wrap around");
    }

    #[test]
    fn arrow_keys_move_the_selection_and_stop_at_the_ends() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active, Queued, Paused]);
        app.handle_event(&key(KeyCode::Down));
        app.handle_event(&key(KeyCode::Down));
        app.handle_event(&key(KeyCode::Down));
        assert_eq!(app.selected.as_deref(), Some("dl_3"));
        app.handle_event(&key(KeyCode::Home));
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
    }

    #[test]
    fn action_keys_target_the_selected_download() {
        let mut app = app_with(&[DownloadStatus::Active]);
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('p'))),
            Some(Action::Pause("dl_1".to_string()))
        );
        assert_eq!(
            app.handle_event(&key(KeyCode::Delete)),
            Some(Action::Remove("dl_1".to_string()))
        );
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn actions_without_a_selection_explain_instead_of_failing() {
        let mut app = App::default();
        assert_eq!(app.handle_event(&key(KeyCode::Char('p'))), None);
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn typing_or_pasting_a_url_then_enter_adds_it() {
        let mut app = App::default();
        app.handle_event(&key(KeyCode::Char('a')));
        for c in "http://x/a".chars() {
            app.handle_event(&key(KeyCode::Char(c)));
        }
        app.handle_event(&key(KeyCode::Backspace));
        app.handle_event(&Event::Paste("b.iso\n".to_string()));
        assert_eq!(
            app.handle_event(&key(KeyCode::Enter)),
            Some(Action::Add("http://x/b.iso".to_string()))
        );
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn letters_typed_into_the_url_box_are_not_shortcuts() {
        let mut app = app_with(&[DownloadStatus::Active]);
        app.handle_event(&key(KeyCode::Char('a')));
        assert_eq!(app.handle_event(&key(KeyCode::Char('q'))), None);
        assert_eq!(app.mode, Mode::AddUrl("q".to_string()));
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn pasting_while_browsing_opens_the_add_box() {
        let mut app = App::default();
        app.handle_event(&Event::Paste("  http://x/y  ".to_string()));
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

        app.handle_event(&click(3, 6));
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
        // The Cancel button asks first; "y" confirms.
        assert_eq!(app.handle_event(&click(2, 1)), None);
        assert_eq!(app.mode, Mode::ConfirmCancel("dl_2".to_string()));
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('y'))),
            Some(Action::Cancel("dl_2".to_string()))
        );
        app.handle_event(&click(12, 2));
        assert_eq!(app.tab, Tab::Queued);
        assert_eq!(
            app.handle_event(&click(70, 30)),
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
        app.handle_event(&scroll(MouseEventKind::ScrollDown));
        assert_eq!(app.selected.as_deref(), Some("dl_2"));
        app.handle_event(&scroll(MouseEventKind::ScrollUp));
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
    }

    #[test]
    fn speed_history_is_capped() {
        let mut app = App::default();
        for i in 0..(SPEED_HISTORY_LEN as u64 + 5) {
            app.apply_tick(TelemetryTick {
                id: "dl_1".to_string(),
                downloaded_bytes: 0,
                total_bytes: None,
                speed_bps: i,
                active_connections: 1,
                segments: Vec::new(),
                lanes: Vec::new(),
            });
        }
        let history = &app.speed_history["dl_1"];
        assert_eq!(history.len(), SPEED_HISTORY_LEN);
        assert_eq!(history.front(), Some(&5));
    }

    #[test]
    fn cancel_asks_first_and_only_y_confirms() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active]);
        assert_eq!(app.handle_event(&key(KeyCode::Char('c'))), None);
        assert_eq!(app.mode, Mode::ConfirmCancel("dl_1".to_string()));
        assert_eq!(
            app.handle_event(&key(KeyCode::Char('y'))),
            Some(Action::Cancel("dl_1".to_string()))
        );
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn any_other_key_or_a_click_backs_out_of_cancelling() {
        use DownloadStatus::*;
        let mut app = app_with(&[Active]);
        app.handle_event(&key(KeyCode::Char('c')));
        assert_eq!(app.handle_event(&key(KeyCode::Char('n'))), None);
        assert_eq!(app.mode, Mode::Browse);

        app.handle_event(&key(KeyCode::Char('c')));
        assert_eq!(app.handle_event(&click(0, 0)), None);
        assert_eq!(app.mode, Mode::Browse);
    }
}
