//! Everything the TUI says to the daemon: reloading the list, performing
//! actions, and listening for telemetry.

use crate::ipc::client::{ClientError, DaemonClient};
use crate::ipc::protocol::{AddDownloadParams, DownloadSummary, QueuePosition, TelemetryTick};
use crate::ipc::transport::IpcEndpoint;
use crate::tui::action::Action;
use crate::tui::format::status_label;
use crate::tui::state::App;
use std::time::Duration;
use tokio::sync::mpsc;

/// Wait before the telemetry listener reconnects after losing the daemon.
const TELEMETRY_RETRY: Duration = Duration::from_secs(1);

/// What a successful action reports, and the download to select after it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub message: String,
    pub select: Option<String>,
}

/// Why an action failed. `code` is the daemon's error code when it sent one;
/// `not_added` lists the URLs an Add could not add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionError {
    pub message: String,
    pub code: Option<i32>,
    pub not_added: Vec<String>,
}

impl From<ClientError> for ActionError {
    fn from(err: ClientError) -> Self {
        let code = match &err {
            ClientError::Rpc(e) => Some(e.code),
            _ => None,
        };
        Self {
            message: err.to_string(),
            code,
            not_added: Vec::new(),
        }
    }
}

/// Forward telemetry ticks from a dedicated connection, reconnecting if the daemon restarts.
pub(crate) async fn listen_for_telemetry(
    endpoint: IpcEndpoint,
    ticks: mpsc::Sender<TelemetryTick>,
) {
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
pub(crate) async fn refresh(
    client: &mut Option<DaemonClient>,
    endpoint: &IpcEndpoint,
    app: &mut App,
) {
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
            // An rdmd from before defaults existed has none to report.
            app.defaults = conn.get_defaults().await.ok();
        }
        Err(e) => {
            app.connected = false;
            app.notify(format!("Lost connection to rdmd ({e}); retrying"), true);
            *client = None;
        }
    }
}

/// Ask the daemon to carry out `action`.
pub(crate) async fn perform(
    client: &mut Option<DaemonClient>,
    action: &Action,
) -> Result<Outcome, ActionError> {
    let Some(conn) = client.as_mut() else {
        return Err(ClientError::NotRunning.into());
    };
    let summary = match action {
        Action::Add(url) => {
            let s = conn
                .add(AddDownloadParams {
                    url: url.clone(),
                    // The daemon fills in its default folder.
                    destination: None,
                    filename: None,
                    connections: None,
                    max_bytes_per_sec: None,
                })
                .await?;
            return Ok(Outcome {
                message: format!("Added {} ({})", s.id, status_label(s.status)),
                select: Some(s.id),
            });
        }
        Action::Pause(id) => conn.pause(id).await?,
        Action::Resume(id) => conn.resume(id).await?,
        Action::Cancel(id) => conn.cancel(id, false).await?,
        Action::Remove(id) => {
            let s = conn.remove(id).await?;
            return Ok(message(format!("Removed {}", s.id)));
        }
        Action::Move(id, position) => {
            let s = conn.move_download(id, *position).await?;
            return Ok(message(format!(
                "Moved {} {}",
                s.id,
                position_word(*position)
            )));
        }
        Action::Quit => return Ok(Outcome::default()),
    };
    Ok(changed(&summary))
}

fn message(text: String) -> Outcome {
    Outcome {
        message: text,
        select: None,
    }
}

fn changed(s: &DownloadSummary) -> Outcome {
    message(format!("{}: {}", s.id, status_label(s.status)))
}

fn position_word(position: QueuePosition) -> &'static str {
    match position {
        QueuePosition::Top => "to the top",
        QueuePosition::Up => "up",
        QueuePosition::Down => "down",
        QueuePosition::Bottom => "to the bottom",
    }
}
