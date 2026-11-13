//! The dashboard's event loop: terminal events, telemetry and the refresh timer.

use crate::ipc::client::DaemonClient;
use crate::ipc::transport::IpcEndpoint;
use crate::tui::action::Action;
use crate::tui::daemon_link::{listen_for_telemetry, perform, refresh};
use crate::tui::screens::logs;
use crate::tui::state::{App, Screen};
use crate::tui::{screens, TuiResult, REFRESH_INTERVAL};
use ratatui::backend::Backend;
use ratatui::crossterm::event::Event;
use ratatui::Terminal;
use std::io;
use tokio::sync::mpsc;

/// Run the dashboard on any ratatui backend, reading terminal events from `events`.
/// Returns when the user quits or `events` closes.
pub async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    mut events: mpsc::Receiver<Event>,
    endpoint: &IpcEndpoint,
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
        if let Err(e) = terminal.draw(|frame| screens::render(frame, &mut app)) {
            break Err(io::Error::other(e.to_string()).into());
        }
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break Ok(()) };
                let typing = !app.dialogs.is_empty() || app.settings.is_editing();
                match screens::handle_event(&mut app, &event) {
                    Some(Action::Quit) => break Ok(()),
                    Some(action) => {
                        let result = perform(&mut client, &action).await;
                        let stopped = action == Action::StopDaemon && result.is_ok();
                        app.apply_outcome(result);
                        if stopped {
                            break Ok(());
                        }
                        if typing {
                            drop_type_ahead(&mut events);
                        }
                        refresh(&mut client, endpoint, &mut app).await;
                    }
                    None => {}
                }
            }
            Some(tick) = tick_rx.recv() => app.apply_tick(tick),
            _ = refresh_timer.tick() => {
                refresh(&mut client, endpoint, &mut app).await;
                if app.screen == Screen::Logs {
                    logs::reload(&mut app);
                }
            }
        }
    };
    telemetry.abort();
    result
}

/// Drop events that queued up while a dialog's or a field's submit was being
/// carried out. Windows has no paste events; the reader turns a burst of key
/// presses into one paste (see `paste`), but if part of one still arrives as
/// keys after an Enter, the letters would otherwise land on the screen below
/// as shortcuts (`c` cancel, `d` delete).
fn drop_type_ahead(events: &mut mpsc::Receiver<Event>) {
    while events.try_recv().is_ok() {}
}
