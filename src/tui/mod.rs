//! Interactive terminal dashboard (`rdm tui`) and its plain-text fallback.

pub mod action;
#[cfg(windows)]
mod clipboard;
mod cursor_resync;
pub mod daemon_link;
pub mod dialogs;
pub mod format;
pub mod open_folder;
pub mod paste;
pub mod runtime;
pub mod screens;
pub mod state;
pub mod theme;
pub mod widgets;

use crate::ipc::client::DaemonClient;
use crate::ipc::protocol::DownloadStatus;
use crate::ipc::transport::IpcEndpoint;
use cursor_resync::CursorResync;
use format::format_row;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;
use tokio::sync::mpsc;

pub use crate::dirs::default_downloads_dir;
pub use runtime::run_app;

/// How often the dashboard re-reads the download list and limits.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// How often plain mode prints rows that changed.
pub const PLAIN_INTERVAL: Duration = Duration::from_secs(1);

pub type TuiResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Run the dashboard in the real terminal: raw mode, alternate screen, mouse
/// capture and bracketed paste, all undone on exit or panic.
pub async fn run_interactive(endpoint: &IpcEndpoint) -> TuiResult<()> {
    install_restore_hook();
    let mut terminal = enter_terminal()?;
    let (tx, rx) = mpsc::channel(64);
    // crossterm's reader blocks, so it gets its own thread. It ends with the process.
    std::thread::spawn(move || {
        while let Ok(batch) = read_batch() {
            for event in batch {
                if tx.blocking_send(event).is_err() {
                    return;
                }
            }
        }
    });

    let result = run_app(&mut terminal, rx, endpoint).await;
    restore_terminal();
    let _ = terminal.show_cursor();
    result
}

/// Longest pause between events that still counts as one burst. Pasted keys
/// arrive together; typed ones are tens of milliseconds apart.
#[cfg(windows)]
const BURST_GAP: Duration = Duration::from_millis(2);
/// Longest a batch is gathered, so nothing waits long behind a burst.
#[cfg(windows)]
const MAX_BATCH_TIME: Duration = Duration::from_millis(50);

/// Windows terminals send no paste events, so gather a burst of typed keys
/// that arrive together; the first other event (a shortcut, the mouse, a
/// resize) ends the batch at once. Windows Terminal pastes Ctrl+V as such a
/// burst, but the old console hands Ctrl+V to rdm, so that reads the clipboard.
#[cfg(windows)]
fn read_batch() -> io::Result<Vec<event::Event>> {
    let started = std::time::Instant::now();
    let mut batch = vec![event::read()?];
    while batch.last().is_some_and(paste::continues_burst)
        && started.elapsed() < MAX_BATCH_TIME
        && event::poll(BURST_GAP)?
    {
        batch.push(event::read()?);
    }
    Ok(paste::coalesce_paste(batch)
        .into_iter()
        .map(|event| paste::expand_paste_shortcut(event, clipboard::read_text))
        .collect())
}

/// Elsewhere the terminal reports a paste as one event (bracketed paste).
#[cfg(not(windows))]
fn read_batch() -> io::Result<Vec<event::Event>> {
    Ok(vec![event::read()?])
}

fn enter_terminal() -> io::Result<Terminal<CursorResync<CrosstermBackend<io::Stdout>>>> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Terminal::new(CursorResync::new(CrosstermBackend::new(io::stdout())))
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

fn in_flight(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Active
            | DownloadStatus::Queued
            | DownloadStatus::Pausing
            | DownloadStatus::Retrying
    )
}

/// What plain mode tells the user when a video page waits for its quality.
const CHOOSE_HINT: &str = "This is a video page; choose a quality in the dashboard (rdm) or remove it and add it again with --quality 1080p.";

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
            // Nothing more will happen on its own: say what a waiting video needs.
            if downloads
                .iter()
                .any(|d| d.status == DownloadStatus::Choosing)
            {
                writeln!(out, "{CHOOSE_HINT}")?;
            }
            return Ok(());
        }
        tokio::time::sleep(interval).await;
    }
}
