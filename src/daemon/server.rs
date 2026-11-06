use crate::daemon::core::DaemonCore;
use crate::ipc::protocol::*;
use crate::ipc::transport::{IpcEndpoint, IpcListener, IpcTransportError};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn parse_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    let params = params.ok_or_else(|| RpcError::invalid_params("Missing params"))?;
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn respond<T: Serialize>(id: RpcId, result: Result<T, RpcError>) -> RpcResponse {
    let value = result
        .and_then(|v| serde_json::to_value(v).map_err(|e| RpcError::internal_error(e.to_string())));
    match value {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, e),
    }
}

impl DaemonCore {
    pub fn handle_request(&self, req: RpcRequest) -> RpcResponse {
        let id = req.id;
        let params = req.params;
        match req.method.as_str() {
            "daemon.ping" => {
                let uptime = self.state.lock().unwrap().start_time.elapsed().as_secs();
                respond(
                    id,
                    Ok(PingResult {
                        version: crate::VERSION.to_string(),
                        uptime_secs: uptime,
                    }),
                )
            }
            "daemon.shutdown" => {
                self.shutdown();
                respond(
                    id,
                    Ok(ShutdownResult {
                        status: "shutting_down".to_string(),
                    }),
                )
            }
            "daemon.get_limits" => respond(id, Ok(self.limits())),
            "daemon.set_limits" => {
                respond(id, parse_params(params).and_then(|p| self.set_limits(p)))
            }
            "download.add" => respond(id, parse_params(params).and_then(|p| self.add_download(p))),
            "download.pause" => respond(
                id,
                parse_params::<DownloadIdParam>(params).and_then(|p| self.pause_download(&p.id)),
            ),
            "download.resume" => respond(
                id,
                parse_params::<DownloadIdParam>(params).and_then(|p| self.resume_download(&p.id)),
            ),
            "download.cancel" => respond(
                id,
                parse_params(params).and_then(|p| self.cancel_download(p)),
            ),
            "download.remove" => respond(
                id,
                parse_params::<DownloadIdParam>(params).and_then(|p| self.remove_download(&p.id)),
            ),
            "download.move" => {
                respond(id, parse_params(params).and_then(|p| self.move_download(p)))
            }
            "download.set_speed" => respond(
                id,
                parse_params(params).and_then(|p| self.set_download_speed(p)),
            ),
            "download.list" => respond(id, Ok(self.list_downloads())),
            "download.status" => respond(
                id,
                parse_params::<DownloadIdParam>(params).and_then(|p| self.get_status(&p.id)),
            ),
            "subscribe.telemetry" => respond(id, Ok(serde_json::json!({ "subscribed": true }))),
            other => RpcResponse::error(id, RpcError::method_not_found(other)),
        }
    }
}

/// Standalone Server driving the IPC listener and background workers.
pub struct DaemonServer {
    pub listener: IpcListener,
    pub core: DaemonCore,
}

impl DaemonServer {
    /// Bind a server whose queue lives in memory only.
    pub async fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        Self::bind_with_core(endpoint, DaemonCore::new()).await
    }

    pub async fn bind_with_core(
        endpoint: &IpcEndpoint,
        core: DaemonCore,
    ) -> Result<Self, IpcTransportError> {
        let listener = IpcListener::bind(endpoint).await?;
        Ok(Self { listener, core })
    }

    pub async fn run(mut self) -> Result<(), IpcTransportError> {
        let core = self.core.clone();
        let shutdown_notify = Arc::clone(&core.shutdown_notify);

        // Downloads restored from disk start as soon as the daemon is up.
        core.schedule();

        // Background ticker task
        let ticker_core = core.clone();
        let ticker_notify = Arc::clone(&shutdown_notify);
        let ticker_handle = tokio::spawn(async move {
            // Register for the notification before checking the flag, so a shutdown
            // that lands between loop iterations (or before this task starts) is not lost.
            let shutdown = ticker_notify.notified();
            tokio::pin!(shutdown);
            shutdown.as_mut().enable();
            if ticker_core.is_shutting_down() {
                return;
            }

            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        ticker_core.tick_telemetry();
                    }
                    _ = &mut shutdown => {
                        break;
                    }
                }
            }
        });

        let shutdown = shutdown_notify.notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();

        while !core.is_shutting_down() {
            tokio::select! {
                accept_res = self.listener.accept() => {
                    let conn = match accept_res {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("Failed to accept IPC connection: {}", e);
                            continue;
                        }
                    };

                    let client_core = core.clone();
                    let mut telemetry_rx = core.telemetry_tx.subscribe();

                    tokio::spawn(async move {
                        let (mut sender, mut receiver) = conn.into_split();
                        let mut is_subscribed = false;

                        loop {
                            tokio::select! {
                                msg_opt = receiver.recv() => {
                                    match msg_opt {
                                        Ok(Some(RpcMessage::Request(req))) => {
                                            if req.method == "subscribe.telemetry" {
                                                is_subscribed = true;
                                            }
                                            let resp = client_core.handle_request(req);
                                            if let Err(e) = sender.send(&resp).await {
                                                tracing::debug!("Failed to send RPC response: {}", e);
                                                break;
                                            }
                                        }
                                        Ok(Some(RpcMessage::Notification(_))) => {}
                                        Ok(Some(RpcMessage::Response(_))) => {}
                                        Ok(None) => break,
                                        Err(e) => {
                                            tracing::debug!("IPC client read error: {}", e);
                                            break;
                                        }
                                    }
                                }
                                tick_res = telemetry_rx.recv(), if is_subscribed => {
                                    if let Ok(tick) = tick_res {
                                        let notif = RpcNotification::new(
                                            "telemetry.tick",
                                            Some(serde_json::to_value(&tick).unwrap()),
                                        );
                                        if let Err(e) = sender.send(&notif).await {
                                            tracing::debug!("Failed to send telemetry tick: {}", e);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                _ = &mut shutdown => {
                    break;
                }
            }
        }

        let _ = ticker_handle.await;
        Ok(())
    }
}
