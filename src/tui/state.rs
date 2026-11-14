//! The TUI's state: what the daemon reported, what the user selected, and
//! what the last frame drew.

use crate::ipc::protocol::{
    DaemonLimits, DownloadDefaults, DownloadStatus, DownloadSummary, TelemetryTick,
};
use crate::tui::daemon_link::{ActionError, Outcome};
use crate::tui::dialogs::{Dialog, DialogTarget};
use crate::tui::screens::logs::LogsState;
use crate::tui::screens::settings::{SettingsField, SettingsState};
use ratatui::layout::Rect;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// Speed samples kept per download for the speed graph: about 30 s at 4 ticks a second.
pub const SPEED_HISTORY_LEN: usize = 120;
/// Notices kept for the Logs screen.
pub const NOTICE_HISTORY: usize = 200;

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

    pub(crate) fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub(crate) fn offset(self, delta: isize) -> Tab {
        let len = Tab::ALL.len() as isize;
        let next = (self.index() as isize + delta).rem_euclid(len);
        Tab::ALL[next as usize]
    }
}

/// The TUI's screens, in the order the header lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Downloads,
    Settings,
    Logs,
}

impl Screen {
    pub const ALL: [Screen; 3] = [Screen::Downloads, Screen::Settings, Screen::Logs];

    pub fn title(self) -> &'static str {
        match self {
            Screen::Downloads => "Downloads",
            Screen::Settings => "Settings",
            Screen::Logs => "Logs",
        }
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
    Speed,
    Quit,
}

impl Button {
    pub const ALL: [Button; 7] = [
        Button::Add,
        Button::Pause,
        Button::Resume,
        Button::Cancel,
        Button::Remove,
        Button::Speed,
        Button::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Button::Add => "+ Add",
            Button::Pause => "|| Pause",
            Button::Resume => "> Resume",
            Button::Cancel => "x Cancel",
            Button::Remove => "- Remove",
            Button::Speed => "~ Speed",
            Button::Quit => "Quit",
        }
    }
}

/// Screen areas the last frame drew, so a mouse click can find what it hit.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    /// The screen names in the header.
    pub screens: Vec<(Rect, Screen)>,
    pub buttons: Vec<(Rect, Button)>,
    pub tabs: Vec<(Rect, Tab)>,
    /// One entry per table row on screen, with that row's download id.
    pub rows: Vec<(Rect, String)>,
    /// Fields and buttons of the top dialog.
    pub dialog: Vec<(Rect, DialogTarget)>,
    /// One row per field on the Settings screen.
    pub settings: Vec<(Rect, SettingsField)>,
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
    /// What new downloads start with, as the daemon reports it.
    pub defaults: Option<DownloadDefaults>,
    pub connected: bool,
    pub screen: Screen,
    /// The Help overlay is drawn over the current screen.
    pub help_open: bool,
    /// Open dialogs; the last one is on top and gets the keys.
    pub dialogs: Vec<Dialog>,
    pub settings: SettingsState,
    pub logs: LogsState,
    /// rdmd's log file, as its `ping` reports it.
    pub log_path: Option<PathBuf>,
    pub tab: Tab,
    pub selected: Option<String>,
    /// Index of the first visible table row.
    pub table_offset: usize,
    pub notice: Option<Notice>,
    /// Every notice shown, oldest first, up to `NOTICE_HISTORY`.
    pub history: VecDeque<Notice>,
    pub hits: HitMap,
}

impl Default for App {
    fn default() -> Self {
        Self {
            downloads: Vec::new(),
            ticks: HashMap::new(),
            speed_history: HashMap::new(),
            limits: None,
            defaults: None,
            connected: false,
            screen: Screen::Downloads,
            help_open: false,
            dialogs: Vec::new(),
            settings: SettingsState::default(),
            logs: LogsState::default(),
            log_path: None,
            tab: Tab::All,
            selected: None,
            table_offset: 0,
            notice: None,
            history: VecDeque::new(),
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

    /// Why `d` has had no new data for a while, while it is active.
    pub fn stall_of(&self, d: &DownloadSummary) -> Option<&str> {
        if d.status != DownloadStatus::Active {
            return None;
        }
        self.ticks.get(&d.id)?.stall.as_deref()
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
        let notice = Notice {
            text: text.into(),
            is_error,
        };
        if self.history.len() == NOTICE_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(notice.clone());
        self.notice = Some(notice);
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

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.selected = Some(visible[next].id.clone());
    }

    /// Show the result of an action: select what it names and report it.
    /// A dialog waiting on the action closes when it worked, and shows the
    /// error and stays open when it didn't. A Settings save works the same way.
    pub fn apply_outcome(&mut self, result: Result<Outcome, ActionError>) {
        let waiting = self.dialogs.last().is_some_and(Dialog::is_pending);
        match result {
            Err(e) if waiting => {
                if let Some(dialog) = self.dialogs.last_mut() {
                    dialog.set_error(&e);
                }
            }
            Err(e) if self.settings.pending => self.settings.set_error(&e),
            Ok(outcome) => {
                if waiting {
                    self.dialogs.pop();
                } else if self.settings.pending {
                    self.settings.saved();
                }
                if let Some(id) = outcome.select {
                    self.selected = Some(id);
                }
                if !outcome.message.is_empty() {
                    self.notify(outcome.message, false);
                }
            }
            Err(e) => self.notify(e.message, true),
        }
    }
}

/// Builders shared by the TUI's unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::ipc::protocol::SegmentProgress;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    pub(crate) fn summary(id: &str, status: DownloadStatus) -> DownloadSummary {
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

    pub(crate) fn app_with(statuses: &[DownloadStatus]) -> App {
        let mut app = App::default();
        let downloads = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| summary(&format!("dl_{}", i + 1), *s))
            .collect();
        app.set_downloads(downloads);
        app
    }

    pub(crate) fn sample_app() -> App {
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
            stall: None,
        });
        app
    }

    pub(crate) fn screen_text(terminal: &Terminal<TestBackend>) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

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
    fn only_an_active_download_reports_a_stall() {
        let mut app = sample_app();
        let reason = "no data for 42s: server busy (HTTP 503), retrying in 8s";
        app.ticks.get_mut("dl_1").unwrap().stall = Some(reason.to_string());
        assert_eq!(app.stall_of(&app.downloads[0]), Some(reason));

        let mut paused = app.downloads[0].clone();
        paused.status = DownloadStatus::Paused;
        app.downloads = vec![paused];
        assert_eq!(
            app.stall_of(&app.downloads[0]),
            None,
            "an old tick is ignored"
        );
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
                stall: None,
            });
        }
        let history = &app.speed_history["dl_1"];
        assert_eq!(history.len(), SPEED_HISTORY_LEN);
        assert_eq!(history.front(), Some(&5));
    }

    #[test]
    fn an_outcome_selects_the_new_download_and_an_error_becomes_a_notice() {
        let mut app = App::default();
        app.apply_outcome(Ok(Outcome {
            message: "Added dl_1 (queued)".into(),
            select: Some("dl_1".into()),
        }));
        assert_eq!(app.selected.as_deref(), Some("dl_1"));
        app.apply_outcome(Err(ActionError {
            message: "boom".into(),
            code: None,
            not_added: vec![],
        }));
        assert_eq!(
            app.notice,
            Some(Notice {
                text: "boom".into(),
                is_error: true
            })
        );
    }
}
