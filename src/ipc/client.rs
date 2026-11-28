use crate::ipc::protocol::*;
use crate::ipc::transport::{IpcConnection, IpcEndpoint, IpcTransportError};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io;
use std::time::{Duration, Instant};

/// How often `connect_or_spawn` retries while a freshly spawned daemon starts up.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("rdmd is not running")]
    NotRunning,
    #[error("{}", .0.message)]
    Rpc(RpcError),
    #[error("rdmd closed the connection")]
    ConnectionClosed,
    #[error("failed to start rdmd: {0}")]
    Spawn(io::Error),
    #[error("rdmd did not start listening within {0:?}; run `rdmd` in a terminal to see why")]
    SpawnTimeout(Duration),
    #[error("unexpected reply from rdmd: {0}")]
    Decode(#[from] serde_json::Error),
    #[error(transparent)]
    Transport(#[from] IpcTransportError),
}

impl ClientError {
    /// True when the daemon went away, which is the expected result of a shutdown.
    fn is_disconnect(&self) -> bool {
        match self {
            Self::ConnectionClosed => true,
            Self::Transport(IpcTransportError::Io(e)) => matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::UnexpectedEof
            ),
            _ => false,
        }
    }
}

/// JSON-RPC client for a running `rdmd`.
pub struct DaemonClient {
    conn: IpcConnection,
    next_id: i64,
}

impl DaemonClient {
    /// Connect to the daemon, returning `ClientError::NotRunning` when nothing listens.
    pub async fn connect(endpoint: &IpcEndpoint) -> Result<Self, ClientError> {
        match IpcConnection::connect(endpoint).await {
            Ok(conn) => Ok(Self { conn, next_id: 1 }),
            Err(IpcTransportError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Err(ClientError::NotRunning)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Send one request and wait for its response, skipping telemetry notifications.
    pub async fn call<R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<R, ClientError> {
        let id = RpcId::Number(self.next_id);
        self.next_id += 1;
        self.conn
            .send(&RpcRequest::new(id.clone(), method, params))
            .await?;

        loop {
            match self.conn.recv().await? {
                None => return Err(ClientError::ConnectionClosed),
                Some(RpcMessage::Response(resp)) if resp.id == id => {
                    if let Some(err) = resp.error {
                        return Err(ClientError::Rpc(err));
                    }
                    return Ok(serde_json::from_value(resp.result.unwrap_or(Value::Null))?);
                }
                Some(_) => continue,
            }
        }
    }

    pub async fn ping(&mut self) -> Result<PingResult, ClientError> {
        self.call("daemon.ping", None).await
    }

    /// Ask the daemon to stop. A dropped connection counts as success.
    pub async fn shutdown(&mut self) -> Result<(), ClientError> {
        match self.call::<ShutdownResult>("daemon.shutdown", None).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_disconnect() => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn add(&mut self, params: AddDownloadParams) -> Result<DownloadSummary, ClientError> {
        self.call("download.add", Some(serde_json::to_value(params)?))
            .await
    }

    /// Pick the quality of a download that is waiting for one.
    pub async fn choose_quality(
        &mut self,
        params: ChooseQualityParams,
    ) -> Result<DownloadSummary, ClientError> {
        self.call(
            "download.choose_quality",
            Some(serde_json::to_value(params)?),
        )
        .await
    }

    /// Look a video up for the browser without adding a download.
    pub async fn look_up(&mut self, params: LookUpParams) -> Result<LookUpResult, ClientError> {
        self.call("video.look_up", Some(serde_json::to_value(params)?))
            .await
    }

    pub async fn list(&mut self) -> Result<Vec<DownloadSummary>, ClientError> {
        self.call("download.list", None).await
    }

    pub async fn status(&mut self, id: &str) -> Result<DownloadSummary, ClientError> {
        self.call("download.status", Some(id_param(id)?)).await
    }

    pub async fn pause(&mut self, id: &str) -> Result<DownloadSummary, ClientError> {
        self.call("download.pause", Some(id_param(id)?)).await
    }

    pub async fn resume(&mut self, id: &str) -> Result<DownloadSummary, ClientError> {
        self.call("download.resume", Some(id_param(id)?)).await
    }

    /// Stop a download for good. With `delete_partial`, its partial files are removed too.
    pub async fn cancel(
        &mut self,
        id: &str,
        delete_partial: bool,
    ) -> Result<DownloadSummary, ClientError> {
        let params = CancelDownloadParams {
            id: id.to_string(),
            delete_partial,
        };
        self.call("download.cancel", Some(serde_json::to_value(params)?))
            .await
    }

    /// Forget a download that is not running. Its files are left alone.
    pub async fn remove(&mut self, id: &str) -> Result<DownloadSummary, ClientError> {
        self.call("download.remove", Some(id_param(id)?)).await
    }

    /// Move a download within the queue.
    pub async fn move_download(
        &mut self,
        id: &str,
        position: QueuePosition,
    ) -> Result<DownloadSummary, ClientError> {
        let params = MoveDownloadParams {
            id: id.to_string(),
            position,
        };
        self.call("download.move", Some(serde_json::to_value(params)?))
            .await
    }

    /// Cap one download's speed in bytes per second; 0 removes the cap.
    pub async fn set_download_speed(
        &mut self,
        id: &str,
        max_bytes_per_sec: u64,
    ) -> Result<DownloadSummary, ClientError> {
        let params = SetDownloadSpeedParams {
            id: id.to_string(),
            max_bytes_per_sec,
        };
        self.call("download.set_speed", Some(serde_json::to_value(params)?))
            .await
    }

    pub async fn get_limits(&mut self) -> Result<DaemonLimits, ClientError> {
        self.call("daemon.get_limits", None).await
    }

    pub async fn set_limits(
        &mut self,
        params: SetLimitsParams,
    ) -> Result<DaemonLimits, ClientError> {
        self.call("daemon.set_limits", Some(serde_json::to_value(params)?))
            .await
    }

    pub async fn get_defaults(&mut self) -> Result<DownloadDefaults, ClientError> {
        self.call("daemon.get_defaults", None).await
    }

    pub async fn set_defaults(
        &mut self,
        params: SetDefaultsParams,
    ) -> Result<DownloadDefaults, ClientError> {
        self.call("daemon.set_defaults", Some(serde_json::to_value(params)?))
            .await
    }

    /// Every helper tool's state: version, previous version, last check, progress.
    pub async fn tools_status(&mut self) -> Result<Vec<ToolInfo>, ClientError> {
        self.call("tools.status", None).await
    }

    /// Check one tool for a newer version (installing it if missing), or every
    /// installed tool. Returns at once; the work shows up in `tools_status`.
    pub async fn tools_update(
        &mut self,
        tool: Option<ToolName>,
    ) -> Result<Vec<ToolInfo>, ClientError> {
        let params = ToolUpdateParams { tool };
        self.call("tools.update", Some(serde_json::to_value(params)?))
            .await
    }

    /// Go back to the previous version of a tool.
    pub async fn tools_rollback(&mut self, tool: ToolName) -> Result<ToolInfo, ClientError> {
        let params = ToolRollbackParams { tool };
        self.call("tools.rollback", Some(serde_json::to_value(params)?))
            .await
    }

    pub async fn subscribe_telemetry(&mut self) -> Result<(), ClientError> {
        self.call::<Value>("subscribe.telemetry", None).await?;
        Ok(())
    }

    /// Wait for the next telemetry tick after `subscribe_telemetry`. Returns
    /// `Ok(None)` when the daemon closes the connection. Use a connection that
    /// makes no other calls, because `call` skips ticks while it waits.
    pub async fn next_tick(&mut self) -> Result<Option<TelemetryTick>, ClientError> {
        loop {
            match self.conn.recv().await? {
                None => return Ok(None),
                Some(RpcMessage::Notification(n)) if n.method == "telemetry.tick" => {
                    let params = n.params.unwrap_or(Value::Null);
                    return Ok(Some(serde_json::from_value(params)?));
                }
                Some(_) => continue,
            }
        }
    }
}

fn id_param(id: &str) -> Result<Value, serde_json::Error> {
    serde_json::to_value(DownloadIdParam { id: id.to_string() })
}

/// Connect to the daemon, calling `spawn` to start it when nothing is listening yet.
pub async fn connect_or_spawn<F>(
    endpoint: &IpcEndpoint,
    spawn: F,
    timeout: Duration,
) -> Result<DaemonClient, ClientError>
where
    F: FnOnce() -> io::Result<()>,
{
    match DaemonClient::connect(endpoint).await {
        Err(ClientError::NotRunning) => {}
        other => return other,
    }

    spawn().map_err(ClientError::Spawn)?;
    let deadline = Instant::now() + timeout;
    loop {
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
        match DaemonClient::connect(endpoint).await {
            Err(ClientError::NotRunning) if Instant::now() < deadline => continue,
            Err(ClientError::NotRunning) => return Err(ClientError::SpawnTimeout(timeout)),
            other => return other,
        }
    }
}
