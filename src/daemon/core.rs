use crate::engine::{self, build_client, DownloadRequest, Outcome, Progress};
use crate::ipc::protocol::*;
use crate::ipc::transport::{IpcEndpoint, IpcListener, IpcTransportError};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use url::Url;

/// In-memory representation of a tracked download task.
pub struct TaskRecord {
    pub id: String,
    pub url: String,
    pub destination: PathBuf,
    pub filename: String,
    pub status: DownloadStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub progress: Arc<Progress>,
    pub cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
    pub error_message: Option<String>,
    pub state_file: Option<PathBuf>,
    pub last_tick_bytes: u64,
    pub last_tick_time: Instant,
    pub connections: u32,
}

impl TaskRecord {
    pub fn to_summary(&self) -> DownloadSummary {
        DownloadSummary {
            id: self.id.clone(),
            url: self.url.clone(),
            destination: self.destination.clone(),
            filename: self.filename.clone(),
            status: self.status,
            downloaded_bytes: self.downloaded_bytes,
            total_bytes: self.total_bytes,
            speed_bps: self.speed_bps,
            eta_secs: self.eta_secs,
            error_message: self.error_message.clone(),
        }
    }
}

pub struct DaemonState {
    pub downloads: HashMap<String, Arc<Mutex<TaskRecord>>>,
    pub next_id: u64,
    pub start_time: Instant,
    pub is_shutting_down: bool,
}

#[derive(Clone)]
pub struct DaemonCore {
    pub state: Arc<Mutex<DaemonState>>,
    pub client: reqwest::Client,
    pub telemetry_tx: broadcast::Sender<TelemetryTick>,
    pub shutdown_notify: Arc<tokio::sync::Notify>,
}

impl Default for DaemonCore {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonCore {
    pub fn new() -> Self {
        let (telemetry_tx, _) = broadcast::channel(1024);
        Self {
            state: Arc::new(Mutex::new(DaemonState {
                downloads: HashMap::new(),
                next_id: 0,
                start_time: Instant::now(),
                is_shutting_down: false,
            })),
            client: build_client(),
            telemetry_tx,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.is_shutting_down = true;
        for task_lock in state.downloads.values() {
            let mut task = task_lock.lock().unwrap();
            if let Some(ref tx) = task.cancel_tx {
                let _ = tx.send(true);
            }
            if task.status == DownloadStatus::Active {
                task.status = DownloadStatus::Pausing;
            }
        }
        self.shutdown_notify.notify_waiters();
    }

    pub fn add_download(&self, params: AddDownloadParams) -> Result<DownloadSummary, RpcError> {
        let parsed_url = Url::parse(&params.url)
            .map_err(|e| RpcError::invalid_params(format!("Invalid URL: {}", e)))?;

        let mut state = self.state.lock().unwrap();
        if state.is_shutting_down {
            return Err(RpcError::daemon_shutting_down());
        }

        // Check for duplicate active/queued URL
        for task_lock in state.downloads.values() {
            let task = task_lock.lock().unwrap();
            if task.url == params.url
                && (task.status == DownloadStatus::Active || task.status == DownloadStatus::Queued)
            {
                return Err(RpcError::duplicate_download(&params.url, &task.id));
            }
        }

        state.next_id += 1;
        let id = format!("dl_{}", state.next_id);

        let downloads_dir = params
            .destination
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let template = match params.filename {
            Some(ref f) => format!("{{downloads}}/{}", f),
            None => "{downloads}/{name}".to_string(),
        };

        let connections = params.connections.unwrap_or(8).clamp(1, 16) as u32;
        let progress = Arc::new(Progress::new());
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let task_record = Arc::new(Mutex::new(TaskRecord {
            id: id.clone(),
            url: params.url.clone(),
            destination: downloads_dir.clone(),
            filename: params.filename.unwrap_or_else(|| "download".to_string()),
            status: DownloadStatus::Active,
            total_bytes: None,
            downloaded_bytes: 0,
            speed_bps: 0,
            eta_secs: None,
            progress: Arc::clone(&progress),
            cancel_tx: Some(cancel_tx),
            error_message: None,
            state_file: None,
            last_tick_bytes: 0,
            last_tick_time: Instant::now(),
            connections,
        }));

        state.downloads.insert(id.clone(), Arc::clone(&task_record));
        drop(state);

        let req = DownloadRequest {
            url: parsed_url,
            template,
            downloads_dir,
            connections,
            progress,
            cancel: Some(cancel_rx),
        };

        let client = self.client.clone();
        let record_clone = Arc::clone(&task_record);

        tokio::spawn(async move {
            let outcome = engine::download(&client, req).await;
            let mut rec = record_clone.lock().unwrap();
            match outcome {
                Ok(Outcome::Completed { path, steals: _ }) => {
                    rec.status = DownloadStatus::Completed;
                    rec.destination = path.clone();
                    if let Some(name) = path.file_name() {
                        rec.filename = name.to_string_lossy().to_string();
                    }
                    let total = rec.progress.downloaded.load(Ordering::Relaxed);
                    rec.downloaded_bytes = total;
                    rec.total_bytes = Some(total);
                    rec.speed_bps = 0;
                    rec.eta_secs = Some(0);
                    rec.cancel_tx = None;
                }
                Ok(Outcome::Paused { reason: _, state }) => {
                    rec.status = DownloadStatus::Paused;
                    rec.state_file = Some(state);
                    rec.speed_bps = 0;
                    rec.eta_secs = None;
                    rec.cancel_tx = None;
                }
                Err(e) => {
                    rec.status = DownloadStatus::Failed;
                    rec.error_message = Some(e.to_string());
                    rec.speed_bps = 0;
                    rec.eta_secs = None;
                    rec.cancel_tx = None;
                }
            }
        });

        let rec = task_record.lock().unwrap();
        Ok(rec.to_summary())
    }

    pub fn pause_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let state = self.state.lock().unwrap();
        let task_lock = state
            .downloads
            .get(id)
            .ok_or_else(|| RpcError::download_not_found(id))?;
        let mut task = task_lock.lock().unwrap();

        match task.status {
            DownloadStatus::Completed => Err(RpcError::cannot_pause_completed(id)),
            DownloadStatus::Paused => Ok(task.to_summary()),
            DownloadStatus::Pausing => Ok(task.to_summary()),
            DownloadStatus::Active | DownloadStatus::Queued | DownloadStatus::Retrying => {
                task.status = DownloadStatus::Pausing;
                if let Some(ref tx) = task.cancel_tx {
                    let _ = tx.send(true);
                }
                Ok(task.to_summary())
            }
            DownloadStatus::Failed | DownloadStatus::Cancelled => Ok(task.to_summary()),
        }
    }

    pub fn resume_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let state = self.state.lock().unwrap();
        let task_lock = state
            .downloads
            .get(id)
            .ok_or_else(|| RpcError::download_not_found(id))?;
        let mut task = task_lock.lock().unwrap();

        match task.status {
            DownloadStatus::Completed => Err(RpcError::already_completed(id)),
            DownloadStatus::Active => Ok(task.to_summary()),
            DownloadStatus::Pausing => Err(RpcError::operation_in_progress(id, "pausing")),
            DownloadStatus::Paused
            | DownloadStatus::Failed
            | DownloadStatus::Queued
            | DownloadStatus::Retrying => {
                let state_file = match task.state_file.clone() {
                    Some(p) => p,
                    None => {
                        let candidate = crate::state::part_path(&task.destination);
                        crate::state::state_path(&candidate)
                    }
                };

                if !state_file.exists() {
                    return Err(RpcError::corrupt_state_file(
                        &state_file,
                        "state file missing for resume",
                    ));
                }

                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                task.cancel_tx = Some(cancel_tx);
                task.status = DownloadStatus::Active;
                task.error_message = None;

                let _connections = task.connections;
                let progress = Arc::clone(&task.progress);
                let client = self.client.clone();
                let record_clone = Arc::clone(task_lock);

                tokio::spawn(async move {
                    let outcome = engine::resume_with_options(
                        &client,
                        &state_file,
                        None,
                        Some(progress),
                        Some(cancel_rx),
                    )
                    .await;

                    let mut rec = record_clone.lock().unwrap();
                    match outcome {
                        Ok(Outcome::Completed { path, steals: _ }) => {
                            rec.status = DownloadStatus::Completed;
                            rec.destination = path.clone();
                            if let Some(name) = path.file_name() {
                                rec.filename = name.to_string_lossy().to_string();
                            }
                            let total = rec.progress.downloaded.load(Ordering::Relaxed);
                            rec.downloaded_bytes = total;
                            rec.total_bytes = Some(total);
                            rec.speed_bps = 0;
                            rec.eta_secs = Some(0);
                            rec.cancel_tx = None;
                        }
                        Ok(Outcome::Paused { reason: _, state }) => {
                            rec.status = DownloadStatus::Paused;
                            rec.state_file = Some(state);
                            rec.speed_bps = 0;
                            rec.eta_secs = None;
                            rec.cancel_tx = None;
                        }
                        Err(e) => {
                            rec.status = DownloadStatus::Failed;
                            rec.error_message = Some(e.to_string());
                            rec.speed_bps = 0;
                            rec.eta_secs = None;
                            rec.cancel_tx = None;
                        }
                    }
                });

                Ok(task.to_summary())
            }
            DownloadStatus::Cancelled => Err(RpcError::invalid_request(Some(
                "Cannot resume cancelled download".into(),
            ))),
        }
    }

    pub fn cancel_download(
        &self,
        params: CancelDownloadParams,
    ) -> Result<DownloadSummary, RpcError> {
        let state = self.state.lock().unwrap();
        let task_lock = state
            .downloads
            .get(&params.id)
            .ok_or_else(|| RpcError::download_not_found(&params.id))?;
        let mut task = task_lock.lock().unwrap();

        if let Some(ref tx) = task.cancel_tx {
            let _ = tx.send(true);
        }
        task.status = DownloadStatus::Cancelled;
        task.cancel_tx = None;

        if params.delete_partial {
            let part_file = crate::state::part_path(&task.destination);
            let state_file = crate::state::state_path(&task.destination);
            let _ = std::fs::remove_file(part_file);
            let _ = std::fs::remove_file(state_file);
        }

        Ok(task.to_summary())
    }

    pub fn list_downloads(&self) -> Vec<DownloadSummary> {
        let state = self.state.lock().unwrap();
        state
            .downloads
            .values()
            .map(|t| t.lock().unwrap().to_summary())
            .collect()
    }

    pub fn get_status(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let state = self.state.lock().unwrap();
        let summary = {
            let task_lock = state
                .downloads
                .get(id)
                .ok_or_else(|| RpcError::download_not_found(id))?;
            task_lock.lock().unwrap().to_summary()
        };
        Ok(summary)
    }

    pub fn tick_telemetry(&self) {
        let state = self.state.lock().unwrap();
        let now = Instant::now();

        for task_lock in state.downloads.values() {
            let mut task = task_lock.lock().unwrap();
            if task.status != DownloadStatus::Active {
                continue;
            }

            let current_bytes = task.progress.downloaded.load(Ordering::Relaxed);
            let elapsed = now.duration_since(task.last_tick_time).as_secs_f64();
            if elapsed >= 0.1 {
                let delta = current_bytes.saturating_sub(task.last_tick_bytes);
                let speed = (delta as f64 / elapsed) as u64;
                task.speed_bps = speed;
                task.downloaded_bytes = current_bytes;
                task.last_tick_bytes = current_bytes;
                task.last_tick_time = now;

                if let Some(total) = task.total_bytes {
                    if speed > 0 && total > current_bytes {
                        let remaining = total - current_bytes;
                        task.eta_secs = Some(remaining / speed);
                    } else if total <= current_bytes {
                        task.eta_secs = Some(0);
                    }
                }

                // Segments
                let completed_set = task.progress.completed.lock().unwrap();
                let mut segments = Vec::new();
                for r in completed_set.ranges() {
                    segments.push(SegmentProgress {
                        start: r.start,
                        end: r.end,
                        downloaded_bytes: r.len(),
                        state: "completed".to_string(),
                    });
                }

                let tick = TelemetryTick {
                    id: task.id.clone(),
                    downloaded_bytes: current_bytes,
                    total_bytes: task.total_bytes,
                    speed_bps: speed,
                    active_connections: task.connections as usize,
                    segments,
                };

                let _ = self.telemetry_tx.send(tick);
            }
        }
    }

    pub fn handle_request(&self, req: RpcRequest) -> RpcResponse {
        match req.method.as_str() {
            "daemon.ping" => {
                let uptime = self.state.lock().unwrap().start_time.elapsed().as_secs();
                let result = PingResult {
                    version: crate::VERSION.to_string(),
                    uptime_secs: uptime,
                };
                RpcResponse::success(req.id, serde_json::to_value(&result).unwrap())
            }
            "daemon.shutdown" => {
                self.shutdown();
                RpcResponse::success(
                    req.id,
                    serde_json::to_value(ShutdownResult {
                        status: "shutting_down".to_string(),
                    })
                    .unwrap(),
                )
            }
            "download.add" => {
                let params: AddDownloadParams = match req.params {
                    Some(p) => match serde_json::from_value(p) {
                        Ok(val) => val,
                        Err(e) => {
                            return RpcResponse::error(
                                req.id,
                                RpcError::invalid_params(e.to_string()),
                            )
                        }
                    },
                    None => {
                        return RpcResponse::error(
                            req.id,
                            RpcError::invalid_params("Missing params"),
                        )
                    }
                };

                match self.add_download(params) {
                    Ok(summary) => {
                        RpcResponse::success(req.id, serde_json::to_value(&summary).unwrap())
                    }
                    Err(e) => RpcResponse::error(req.id, e),
                }
            }
            "download.pause" => {
                let params: DownloadIdParam = match req.params {
                    Some(p) => match serde_json::from_value(p) {
                        Ok(val) => val,
                        Err(e) => {
                            return RpcResponse::error(
                                req.id,
                                RpcError::invalid_params(e.to_string()),
                            )
                        }
                    },
                    None => {
                        return RpcResponse::error(
                            req.id,
                            RpcError::invalid_params("Missing params"),
                        )
                    }
                };

                match self.pause_download(&params.id) {
                    Ok(summary) => {
                        RpcResponse::success(req.id, serde_json::to_value(&summary).unwrap())
                    }
                    Err(e) => RpcResponse::error(req.id, e),
                }
            }
            "download.resume" => {
                let params: DownloadIdParam = match req.params {
                    Some(p) => match serde_json::from_value(p) {
                        Ok(val) => val,
                        Err(e) => {
                            return RpcResponse::error(
                                req.id,
                                RpcError::invalid_params(e.to_string()),
                            )
                        }
                    },
                    None => {
                        return RpcResponse::error(
                            req.id,
                            RpcError::invalid_params("Missing params"),
                        )
                    }
                };

                match self.resume_download(&params.id) {
                    Ok(summary) => {
                        RpcResponse::success(req.id, serde_json::to_value(&summary).unwrap())
                    }
                    Err(e) => RpcResponse::error(req.id, e),
                }
            }
            "download.cancel" => {
                let params: CancelDownloadParams = match req.params {
                    Some(p) => match serde_json::from_value(p) {
                        Ok(val) => val,
                        Err(e) => {
                            return RpcResponse::error(
                                req.id,
                                RpcError::invalid_params(e.to_string()),
                            )
                        }
                    },
                    None => {
                        return RpcResponse::error(
                            req.id,
                            RpcError::invalid_params("Missing params"),
                        )
                    }
                };

                match self.cancel_download(params) {
                    Ok(summary) => {
                        RpcResponse::success(req.id, serde_json::to_value(&summary).unwrap())
                    }
                    Err(e) => RpcResponse::error(req.id, e),
                }
            }
            "download.list" => {
                let list = self.list_downloads();
                RpcResponse::success(req.id, serde_json::to_value(&list).unwrap())
            }
            "download.status" => {
                let params: DownloadIdParam = match req.params {
                    Some(p) => match serde_json::from_value(p) {
                        Ok(val) => val,
                        Err(e) => {
                            return RpcResponse::error(
                                req.id,
                                RpcError::invalid_params(e.to_string()),
                            )
                        }
                    },
                    None => {
                        return RpcResponse::error(
                            req.id,
                            RpcError::invalid_params("Missing params"),
                        )
                    }
                };

                match self.get_status(&params.id) {
                    Ok(summary) => {
                        RpcResponse::success(req.id, serde_json::to_value(&summary).unwrap())
                    }
                    Err(e) => RpcResponse::error(req.id, e),
                }
            }
            "subscribe.telemetry" => {
                RpcResponse::success(req.id, serde_json::json!({ "subscribed": true }))
            }
            other => RpcResponse::error(req.id, RpcError::method_not_found(other)),
        }
    }
}

/// Standalone Server driving the IPC listener and background workers.
pub struct DaemonServer {
    pub listener: IpcListener,
    pub core: DaemonCore,
}

impl DaemonServer {
    pub async fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        let listener = IpcListener::bind(endpoint).await?;
        Ok(Self {
            listener,
            core: DaemonCore::new(),
        })
    }

    pub async fn run(mut self) -> Result<(), IpcTransportError> {
        let core = self.core.clone();
        let shutdown_notify = Arc::clone(&core.shutdown_notify);

        // Background ticker task
        let ticker_core = core.clone();
        let ticker_notify = Arc::clone(&shutdown_notify);
        let ticker_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        ticker_core.tick_telemetry();
                    }
                    _ = ticker_notify.notified() => {
                        break;
                    }
                }
            }
        });

        loop {
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
                _ = shutdown_notify.notified() => {
                    break;
                }
            }
        }

        let _ = ticker_handle.await;
        Ok(())
    }
}
