//! Everything the TUI says to the daemon: reloading the list, performing
//! actions, and listening for telemetry.

use crate::ipc::client::{ClientError, DaemonClient};
use crate::ipc::protocol::{AddDownloadParams, DownloadSummary, QueuePosition, TelemetryTick};
use crate::ipc::transport::IpcEndpoint;
use crate::tui::action::Action;
use crate::tui::format::{format_speed_cap, status_label};
use crate::tui::open_folder::open_folder;
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
                // Ask where the log is once per connection; an older rdmd doesn't say.
                app.log_path = conn.ping().await.ok().and_then(|p| p.log_path);
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
    // Opening a folder is local; it works even while rdmd is away.
    if let Action::OpenFolder(path) = action {
        return open_folder(path)
            .map(|()| Outcome::default())
            .map_err(|e| ActionError {
                message: format!("Couldn't open the folder: {e}"),
                code: None,
                not_added: Vec::new(),
            });
    }
    let Some(conn) = client.as_mut() else {
        return Err(ClientError::NotRunning.into());
    };
    let summary = match action {
        Action::Add(downloads) => return add_each(conn, downloads).await,
        Action::Pause(id) => conn.pause(id).await?,
        Action::Resume(id) => conn.resume(id).await?,
        Action::Cancel { id, delete_files } => conn.cancel(id, *delete_files).await?,
        Action::SetSpeed {
            id,
            max_bytes_per_sec,
        } => {
            let s = conn.set_download_speed(id, *max_bytes_per_sec).await?;
            return Ok(message(format!(
                "Speed cap for {}: {}",
                s.id,
                format_speed_cap(s.max_bytes_per_sec)
            )));
        }
        Action::SetLimits(params) => {
            conn.set_limits(params.clone()).await?;
            return Ok(message("Limits saved".to_string()));
        }
        Action::SetDefaults(params) => {
            conn.set_defaults(params.clone()).await?;
            return Ok(message("Defaults for new downloads saved".to_string()));
        }
        Action::StopDaemon => {
            conn.shutdown().await?;
            return Ok(message("rdmd stopped".to_string()));
        }
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
        Action::Quit | Action::OpenFolder(_) => return Ok(Outcome::default()),
    };
    Ok(changed(&summary))
}

/// Add each download in turn. When some fail, the error lists the links that
/// were not added, so the Add dialog can keep only those.
async fn add_each(
    conn: &mut DaemonClient,
    downloads: &[AddDownloadParams],
) -> Result<Outcome, ActionError> {
    let mut added: Vec<DownloadSummary> = Vec::new();
    let mut first_error: Option<ActionError> = None;
    let mut not_added = Vec::new();
    for params in downloads {
        match conn.add(params.clone()).await {
            Ok(summary) => added.push(summary),
            Err(e) => {
                first_error.get_or_insert_with(|| e.into());
                not_added.push(params.url.clone());
            }
        }
    }
    if let Some(first) = first_error {
        let message = match downloads.len() {
            1 => first.message,
            total => format!(
                "{} of {total} not added: {}",
                not_added.len(),
                first.message
            ),
        };
        return Err(ActionError {
            message,
            code: first.code,
            not_added,
        });
    }
    let message = match added.as_slice() {
        [one] => format!("Added {} ({})", one.id, status_label(one.status)),
        many => format!("Added {} downloads", many.len()),
    };
    Ok(Outcome {
        message,
        select: added.first().map(|s| s.id.clone()),
    })
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
