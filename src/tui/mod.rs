//! Interactive terminal dashboard (`rdm tui`) and its plain-text fallback.

pub mod action;
pub mod daemon_link;
pub mod format;
pub mod runtime;
pub mod screens;
pub mod state;
pub mod ui;

use crate::ipc::client::DaemonClient;
use crate::ipc::protocol::DownloadStatus;
use crate::ipc::transport::IpcEndpoint;
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
use std::path::Path;
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
