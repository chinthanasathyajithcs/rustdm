//! The TUI's state: what the daemon reported, what the user selected, and
//! what the last frame drew.

use crate::ipc::protocol::{
    DaemonLimits, DownloadDefaults, DownloadStatus, DownloadSummary, TelemetryTick, ToolInfo,
};
use crate::tui::daemon_link::{ActionError, Outcome};
use crate::tui::dialogs::quality::QualityDialog;
use crate::tui::dialogs::{Dialog, DialogTarget};
use crate::tui::screens::logs::LogsState;
use crate::tui::screens::settings::{SettingsField, SettingsState};
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

/// Shown when an open quality list closes because its download is no longer listed.
const LIST_GONE: &str = "That download is gone.";
/// Shown when an open quality list closes because the quality was chosen elsewhere.
const LIST_CHOSEN: &str = "That video's quality was already chosen.";
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
                DownloadStatus::Active
                    | DownloadStatus::Pausing
                    | DownloadStatus::Retrying
                    | DownloadStatus::Choosing
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
    /// The helper tools' state, as last read while Settings was open.
    pub tools: Vec<ToolInfo>,
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
    /// Ids of the downloads this dashboard added. The first time one of them
    /// waits for a quality, its quality list opens by itself.
    pub own_adds: HashSet<String>,
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
            tools: Vec::new(),
            connected: false,
            screen: Screen::Downloads,
            help_open: false,
            dialogs: Vec::new(),
            settings: SettingsState::default(),
            logs: LogsState::default(),
            log_path: None,
            tab: Tab::All,
            selected: None,
            own_adds: HashSet::new(),
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
        self.close_stale_quality_lists();
        self.open_own_quality_list();
    }

    /// Close a quality list whose download no longer waits for one, saying why.
    /// A list that has sent its choice stays: its own answer closes it.
    fn close_stale_quality_lists(&mut self) {
        let mut notice = None;
        let downloads = &self.downloads;
        self.dialogs.retain(|dialog| {
            let Dialog::Quality(list) = dialog else {
                return true;
            };
            if list.is_pending() {
                return true;
            }
            match downloads.iter().find(|d| d.id == list.id()) {
                None => notice = Some(LIST_GONE),
                Some(d) if d.status != DownloadStatus::Choosing => notice = Some(LIST_CHOSEN),
                Some(_) => return true,
            }
            false
        });
        if let Some(text) = notice {
            self.notify(text, false);
        }
    }

    /// Open the quality list for a download added here that now waits for
    /// one, unless a dialog is already open; it opens on a later refresh then.
    /// An id is used up once its list opens, so Esc never brings it back.
    fn open_own_quality_list(&mut self) {
        let finished = |status| {
            matches!(
                status,
                DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Cancelled
            )
        };
        let downloads = &self.downloads;
        self.own_adds.retain(|id| {
            downloads
                .iter()
                .find(|d| &d.id == id)
                .is_some_and(|d| !finished(d.status))
        });
        // Not over a dialog, and not while the user is on another screen or
        // reading Help, where Enter and typing mean something else.
        let free = self.dialogs.is_empty() && self.screen == Screen::Downloads && !self.help_open;
        if !free {
            return;
        }
        let waiting = self.downloads.iter().find_map(|d| {
            let own = d.status == DownloadStatus::Choosing && self.own_adds.contains(&d.id);
            own.then(|| QualityDialog::new(d, self.defaults.as_ref()))?
        });
        if let Some(dialog) = waiting {
            self.own_adds.remove(dialog.id());
            self.dialogs.push(Dialog::Quality(Box::new(dialog)));
        }
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
                self.own_adds.extend(outcome.added);
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
    use crate::ipc::protocol::{SegmentProgress, VideoSummary};
    use crate::video::{Container, VideoChoice};
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
            video: None,
            step: None,
        }
    }

    fn choice(
        id: &str,
        label: &str,
        container: Container,
        height: Option<u32>,
        size: Option<u64>,
        estimate: bool,
    ) -> VideoChoice {
        VideoChoice {
            id: id.to_string(),
            label: label.to_string(),
            container,
            height,
            size_bytes: size,
            size_is_estimate: estimate,
            streams: Vec::new(),
        }
    }

    /// A video page waiting for a quality: seven rows, the default on 1080p.
    pub(crate) fn choosing_summary(id: &str) -> DownloadSummary {
        const MIB: u64 = 1024 * 1024;
        let choices = vec![
            choice(
                "2160-webm",
                "2160p",
                Container::Webm,
                Some(2160),
                Some(57 * MIB),
                true,
            ),
            choice(
                "1440-webm",
                "1440p",
                Container::Webm,
                Some(1440),
                Some(22 * MIB),
                false,
            ),
            choice(
                "1080-mp4",
                "1080p",
                Container::Mp4,
                Some(1080),
                Some(8 * MIB),
                false,
            ),
            choice(
                "720-mp4",
                "720p",
                Container::Mp4,
                Some(720),
                Some(5 * MIB),
                false,
            ),
            choice(
                "480-mp4",
                "480p",
                Container::Mp4,
                Some(480),
                Some(4 * MIB),
                false,
            ),
            choice("360-mp4", "360p", Container::Mp4, Some(360), None, false),
            choice(
                "audio-m4a",
                "Audio only",
                Container::M4a,
                None,
                Some(2 * MIB),
                false,
            ),
        ];
        DownloadSummary {
            filename: "Rust in 100 Seconds.mp4".to_string(),
            video: Some(VideoSummary {
                title: "Rust in 100 Seconds".to_string(),
                duration_secs: Some(149),
                choices,
                default: Some(2),
                chosen: None,
                size_is_estimate: false,
            }),
            ..summary(id, DownloadStatus::Choosing)
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
            video: None,
            step: None,
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
            step: None,
            pieces: None,
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
    fn choosing_rows_show_choose_quality_in_the_active_tab() {
        let mut app = App::default();
        app.set_downloads(vec![
            summary("dl_1", DownloadStatus::Choosing),
            summary("dl_2", DownloadStatus::Queued),
        ]);
        assert!(Tab::Active.includes(DownloadStatus::Choosing));
        assert!(!Tab::Queued.includes(DownloadStatus::Choosing));
        assert!(!Tab::Stopped.includes(DownloadStatus::Choosing));
        assert_eq!(app.count(Tab::Active), 1);
        app.select_tab(Tab::Active);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|f| crate::tui::screens::render(f, &mut app))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("choose "), "{text}");
    }

    fn added(id: &str) -> Result<Outcome, ActionError> {
        Ok(Outcome {
            message: format!("Added {id}"),
            select: Some(id.into()),
            added: vec![id.into()],
        })
    }

    #[test]
    fn a_link_added_here_opens_its_quality_list_once() {
        use crate::tui::dialogs::DialogResult;
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut app = App::default();
        app.apply_outcome(added("dl_1"));
        // While the page is looked up, nothing opens.
        app.set_downloads(vec![summary("dl_1", DownloadStatus::Active)]);
        assert!(app.dialogs.is_empty());

        app.set_downloads(vec![choosing_summary("dl_1")]);
        let Some(Dialog::Quality(dialog)) = app.dialogs.last_mut() else {
            panic!("the quality list did not open");
        };
        assert_eq!(dialog.id(), "dl_1");
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(dialog.handle_event(&esc), DialogResult::Close);
        app.dialogs.pop();

        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(app.dialogs.is_empty(), "Esc must not bring it back");
    }

    #[test]
    fn a_waiting_quality_list_does_not_open_over_another_dialog() {
        let mut app = App::default();
        app.apply_outcome(added("dl_1"));
        app.dialogs.push(Dialog::Add(Box::new(
            crate::tui::dialogs::add::AddDialog::new(None, None),
        )));
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert_eq!(app.dialogs.len(), 1);

        app.dialogs.clear();
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(matches!(app.dialogs.last(), Some(Dialog::Quality(_))));
    }

    #[test]
    fn a_quality_list_waits_for_the_downloads_screen_with_no_help_or_edit() {
        let mut app = App::default();
        app.apply_outcome(added("dl_1"));

        app.screen = Screen::Settings;
        let enter =
            ratatui::crossterm::event::Event::Key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ));
        crate::tui::screens::handle_event(&mut app, &enter);
        assert!(app.settings.is_editing());
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(app.dialogs.is_empty(), "Settings is being edited");

        app.screen = Screen::Logs;
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(app.dialogs.is_empty(), "Logs is showing");

        app.screen = Screen::Downloads;
        app.help_open = true;
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(app.dialogs.is_empty(), "Help is open");

        app.help_open = false;
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(matches!(app.dialogs.last(), Some(Dialog::Quality(_))));
    }

    fn open_list(app: &mut App) {
        app.apply_outcome(added("dl_1"));
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(matches!(app.dialogs.last(), Some(Dialog::Quality(_))));
    }

    #[test]
    fn an_open_list_closes_when_the_quality_was_chosen_elsewhere() {
        let mut app = App::default();
        open_list(&mut app);
        app.set_downloads(vec![summary("dl_1", DownloadStatus::Queued)]);
        assert!(app.dialogs.is_empty());
        assert_eq!(
            app.notice.map(|n| n.text).as_deref(),
            Some("That video's quality was already chosen.")
        );
    }

    #[test]
    fn an_open_list_closes_when_its_download_is_gone() {
        let mut app = App::default();
        open_list(&mut app);
        app.set_downloads(Vec::new());
        assert!(app.dialogs.is_empty());
        assert_eq!(
            app.notice.map(|n| n.text).as_deref(),
            Some("That download is gone.")
        );
    }

    #[test]
    fn a_list_waiting_for_the_daemon_stays_for_its_own_answer() {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::default();
        open_list(&mut app);
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.dialogs.last_mut().unwrap().handle_event(&enter);
        app.set_downloads(vec![summary("dl_1", DownloadStatus::Queued)]);
        assert_eq!(app.dialogs.len(), 1);
    }

    #[test]
    fn own_adds_forget_downloads_that_are_no_longer_listed() {
        let mut app = App::default();
        app.apply_outcome(added("dl_1"));
        app.set_downloads(vec![summary("dl_1", DownloadStatus::Active)]);
        assert!(app.own_adds.contains("dl_1"));
        app.set_downloads(Vec::new());
        assert!(app.own_adds.is_empty());
    }

    #[test]
    fn a_video_added_elsewhere_waits_for_enter() {
        let mut app = App::default();
        app.set_downloads(vec![choosing_summary("dl_1")]);
        assert!(app.dialogs.is_empty());
    }

    #[test]
    fn a_finished_own_add_is_forgotten() {
        let mut app = App::default();
        app.apply_outcome(added("dl_1"));
        app.set_downloads(vec![summary("dl_1", DownloadStatus::Completed)]);
        assert!(app.own_adds.is_empty());
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
                step: None,
                pieces: None,
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
            ..Default::default()
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
