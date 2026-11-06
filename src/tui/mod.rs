//! Interactive terminal dashboard (`rdm tui`) and its plain-text fallback.

pub mod app;
pub mod format;
pub mod ui;

use crate::ipc::client::DaemonClient;
use crate::ipc::protocol::{AddDownloadParams, DownloadStatus, QueuePosition, TelemetryTick};
use crate::ipc::transport::IpcEndpoint;
use app::{Action, App};
use format::{format_row, status_label};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How often the dashboard re-reads the download list and limits.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// How often plain mode prints rows that changed.
pub const PLAIN_INTERVAL: Duration = Duration::from_secs(1);
/// Wait before the telemetry listener reconnects after losing the daemon.
const TELEMETRY_RETRY: Duration = Duration::from_secs(1);

pub type TuiResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Run the dashboard on any ratatui backend, reading terminal events from `events`.
/// Returns when the user quits or `events` closes.
pub async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    mut events: mpsc::Receiver<Event>,
    endpoint: &IpcEndpoint,
    downloads_dir: &Path,
) -> TuiResult<()> {
    let mut client = Some(DaemonClient::connect(endpoint).await?);
    let mut app = App::default();
    let (tick_tx, mut tick_rx) = mpsc::channel(256);
    let telemetry = tokio::spawn(listen_for_telemetry(endpoint.clone(), tick_tx));
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    // Load the list before the first frame, so it never starts empty and early
    // keys have a download to act on.
    refresh(&mut client, endpoint, &mut app).await;
    refresh_timer.tick().await;

    let result = loop {
        if let Err(e) = terminal.draw(|frame| ui::render(frame, &mut app)) {
            break Err(io::Error::other(e.to_string()).into());
        }
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break Ok(()) };
                match app.handle_event(&event) {
                    Some(Action::Quit) => break Ok(()),
                    Some(action) => {
                        perform(&mut client, &mut app, action, downloads_dir).await;
                        refresh(&mut client, endpoint, &mut app).await;
                    }
                    None => {}
                }
            }
            Some(tick) = tick_rx.recv() => app.apply_tick(tick),
            _ = refresh_timer.tick() => refresh(&mut client, endpoint, &mut app).await,
        }
    };
    telemetry.abort();
    result
}

/// Forward telemetry ticks from a dedicated connection, reconnecting if the daemon restarts.
async fn listen_for_telemetry(endpoint: IpcEndpoint, ticks: mpsc::Sender<TelemetryTick>) {
    loop {
        if let Ok(mut client) = DaemonClient::connect(&endpoint).await {
            if client.subscribe_telemetry().await.is_ok() {
                while let Ok(Some(tick)) = client.next_tick().await {
                    if ticks.send(tick).await.is_err() {
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(TELEMETRY_RETRY).await;
    }
}

/// Reload downloads and limits. A failed call drops the connection; the next
/// refresh tries to reconnect, so the dashboard survives a daemon restart.
async fn refresh(client: &mut Option<DaemonClient>, endpoint: &IpcEndpoint, app: &mut App) {
    if client.is_none() {
        *client = DaemonClient::connect(endpoint).await.ok();
    }
    let Some(conn) = client.as_mut() else {
        app.connected = false;
        return;
    };

    match conn.list().await {
        Ok(list) => {
            if !app.connected {
                app.notice = None;
            }
            app.connected = true;
            app.set_downloads(list);
            if let Ok(limits) = conn.get_limits().await {
                app.limits = Some(limits);
            }
        }
        Err(e) => {
            app.connected = false;
            app.notify(format!("Lost connection to rdmd ({e}); retrying"), true);
            *client = None;
        }
    }
}

async fn perform(
    client: &mut Option<DaemonClient>,
    app: &mut App,
    action: Action,
    downloads_dir: &Path,
) {
    let Some(conn) = client.as_mut() else {
        app.notify("rdmd is not running", true);
        return;
    };
    let changed =
        |s: crate::ipc::protocol::DownloadSummary| format!("{}: {}", s.id, status_label(s.status));
    let result = match action {
        Action::Add(url) => conn
            .add(AddDownloadParams {
                url,
                destination: Some(downloads_dir.to_path_buf()),
                filename: None,
                connections: None,
                max_bytes_per_sec: None,
            })
            .await
            .map(|s| {
                app.selected = Some(s.id.clone());
                format!("Added {} ({})", s.id, status_label(s.status))
            }),
        Action::Pause(id) => conn.pause(&id).await.map(changed),
        Action::Resume(id) => conn.resume(&id).await.map(changed),
        Action::Cancel(id) => conn.cancel(&id, false).await.map(changed),
        Action::Remove(id) => conn.remove(&id).await.map(|s| format!("Removed {}", s.id)),
        Action::Move(id, position) => conn
            .move_download(&id, position)
            .await
            .map(|s| format!("Moved {} {}", s.id, position_word(position))),
        Action::Quit => return,
    };
    match result {
        Ok(text) => app.notify(text, false),
        Err(e) => app.notify(e.to_string(), true),
    }
}

/// Run the dashboard in the real terminal: raw mode, alternate screen, mouse
/// capture and bracketed paste, all undone on exit or panic.
pub async fn run_interactive(endpoint: &IpcEndpoint, downloads_dir: &Path) -> TuiResult<()> {
    install_restore_hook();
    let mut terminal = enter_terminal()?;
    let (tx, rx) = mpsc::channel(64);
    // crossterm's reader blocks, so it gets its own thread. It ends with the process.
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    let result = run_app(&mut terminal, rx, endpoint, downloads_dir).await;
    restore_terminal();
    let _ = terminal.show_cursor();
    result
}

fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
}

/// Put the terminal back before a panic message prints, so it stays readable.
fn install_restore_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn position_word(position: QueuePosition) -> &'static str {
    match position {
        QueuePosition::Top => "to the top",
        QueuePosition::Up => "up",
        QueuePosition::Down => "down",
        QueuePosition::Bottom => "to the bottom",
    }
}

fn in_flight(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Active
            | DownloadStatus::Queued
            | DownloadStatus::Pausing
            | DownloadStatus::Retrying
    )
}

/// Script-friendly mode: print each download's row when it changes, until
/// nothing is running or queued.
pub async fn run_plain<W: Write>(
    client: &mut DaemonClient,
    out: &mut W,
    interval: Duration,
) -> TuiResult<()> {
    let mut printed: HashMap<String, String> = HashMap::new();
    loop {
        let downloads = client.list().await?;
        if downloads.is_empty() {
            writeln!(out, "No downloads.")?;
            return Ok(());
        }
        for d in &downloads {
            let row = format_row(d);
            if printed.get(&d.id) != Some(&row) {
                writeln!(out, "{row}")?;
                printed.insert(d.id.clone(), row);
            }
        }
        out.flush()?;

        if !downloads.iter().any(|d| in_flight(d.status)) {
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }
}

/// Default folder for downloads added from the dashboard.
pub fn default_downloads_dir() -> PathBuf {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Downloads")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("Downloads")
    }
}
