use crate::daemon::queue_store::{QueueSnapshot, QueueStore, QUEUE_VERSION};
use crate::daemon::task::TaskRecord;
use crate::engine::{self, build_client, DownloadRequest, Outcome, Progress};
use crate::error::Error;
use crate::ipc::protocol::*;
use crate::limiter::RateLimiter;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use url::Url;

/// Downloads that run at once unless the user changes it.
pub const DEFAULT_MAX_ACTIVE: usize = 3;
/// Highest accepted `max_active`.
pub const MAX_ACTIVE_LIMIT: usize = 32;
/// Lowest accepted speed cap. Slower caps would make pause and cancel sluggish,
/// because a connection waits out its throttle delay before noticing.
pub const MIN_BYTES_PER_SEC: u64 = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;

/// How often `wait_until_settled` re-checks task states.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct DaemonState {
    pub downloads: HashMap<String, Arc<Mutex<TaskRecord>>>,
    pub next_id: u64,
    pub start_time: Instant,
    pub is_shutting_down: bool,
    pub limits: DaemonLimits,
}

/// Owns every download and the queue rules. Lock order: `state`, then a task.
/// `persist` and `schedule` take the locks themselves, so call them with none held.
#[derive(Clone)]
pub struct DaemonCore {
    pub state: Arc<Mutex<DaemonState>>,
    pub client: reqwest::Client,
    pub telemetry_tx: broadcast::Sender<TelemetryTick>,
    pub shutdown_notify: Arc<tokio::sync::Notify>,
    pub rate_limiter: Arc<RateLimiter>,
    store: Option<Arc<QueueStore>>,
}

impl Default for DaemonCore {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonCore {
    /// A daemon core that keeps its queue in memory only.
    pub fn new() -> Self {
        Self::from_snapshot(None, None)
    }

    /// A daemon core that restores its queue from `store` and saves every change to it.
    pub fn with_store(store: QueueStore) -> std::io::Result<Self> {
        let snapshot = store.load()?;
        if let Some(ref s) = snapshot {
            tracing::info!(
                "restored {} downloads from {}",
                s.downloads.len(),
                store.path().display()
            );
        }
        Ok(Self::from_snapshot(snapshot, Some(store)))
    }

    fn from_snapshot(snapshot: Option<QueueSnapshot>, store: Option<QueueStore>) -> Self {
        let limits = snapshot
            .as_ref()
            .map(|s| s.limits.clone())
            .unwrap_or(DaemonLimits {
                max_active: DEFAULT_MAX_ACTIVE,
                max_bytes_per_sec: 0,
            });
        let rate_limiter = Arc::new(RateLimiter::new(limits.max_bytes_per_sec));

        let mut downloads = HashMap::new();
        let mut next_id = 0;
        if let Some(snapshot) = snapshot {
            next_id = snapshot.next_id;
            for saved in snapshot.downloads {
                next_id = next_id.max(download_id_number(&saved.id));
                let rec = TaskRecord::from_saved(saved, &rate_limiter);
                downloads.insert(rec.id.clone(), Arc::new(Mutex::new(rec)));
            }
        }

        let (telemetry_tx, _) = broadcast::channel(1024);
        Self {
            state: Arc::new(Mutex::new(DaemonState {
                downloads,
                next_id,
                start_time: Instant::now(),
                is_shutting_down: false,
                limits,
            })),
            client: build_client(),
            telemetry_tx,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            rate_limiter,
            store: store.map(Arc::new),
        }
    }

    pub fn shutdown(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.is_shutting_down = true;
            for task_lock in state.downloads.values() {
                let mut task = task_lock.lock().unwrap();
                if !task.running {
                    continue;
                }
                if let Some(ref tx) = task.cancel_tx {
                    let _ = tx.send(true);
                }
                if task.status == DownloadStatus::Active {
                    task.status = DownloadStatus::Pausing;
                    task.resume_after_restart = true;
                }
            }
        }
        self.persist();
        self.shutdown_notify.notify_waiters();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.state.lock().unwrap().is_shutting_down
    }

    /// Wait until no download is still running or saving its state.
    /// Returns false if some were still busy when `timeout` ran out.
    pub async fn wait_until_settled(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.has_running_tasks() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
    }

    fn has_running_tasks(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.downloads.values().any(|t| t.lock().unwrap().running)
    }

    fn get_task(&self, id: &str) -> Result<Arc<Mutex<TaskRecord>>, RpcError> {
        let state = self.state.lock().unwrap();
        state
            .downloads
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::download_not_found(id))
    }

    pub fn add_download(&self, params: AddDownloadParams) -> Result<DownloadSummary, RpcError> {
        Url::parse(&params.url)
            .map_err(|e| RpcError::invalid_params(format!("Invalid URL: {}", e)))?;

        let task = {
            let mut state = self.state.lock().unwrap();
            if state.is_shutting_down {
                return Err(RpcError::daemon_shutting_down());
            }

            for task_lock in state.downloads.values() {
                let task = task_lock.lock().unwrap();
                let unfinished = matches!(
                    task.status,
                    DownloadStatus::Active | DownloadStatus::Queued | DownloadStatus::Pausing
                );
                if task.url == params.url && unfinished {
                    return Err(RpcError::duplicate_download(&params.url, &task.id));
                }
            }

            state.next_id += 1;
            let id = format!("dl_{}", state.next_id);
            let directory = params
                .destination
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let connections = params
                .connections
                .unwrap_or(engine::DEFAULT_CONNECTIONS as usize)
                .clamp(1, MAX_CONNECTIONS) as u32;

            let task = Arc::new(Mutex::new(TaskRecord::new(
                id.clone(),
                params.url,
                directory,
                params.filename,
                connections,
                &self.rate_limiter,
            )));
            state.downloads.insert(id, Arc::clone(&task));
            task
        };

        self.persist();
        self.schedule();
        let summary = task.lock().unwrap().to_summary();
        Ok(summary)
    }

    pub fn pause_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let task_lock = self.get_task(id)?;
        let summary = {
            let mut task = task_lock.lock().unwrap();
            match task.status {
                DownloadStatus::Completed => return Err(RpcError::cannot_pause_completed(id)),
                DownloadStatus::Queued => task.status = DownloadStatus::Paused,
                DownloadStatus::Active | DownloadStatus::Retrying => {
                    task.status = DownloadStatus::Pausing;
                    if let Some(ref tx) = task.cancel_tx {
                        let _ = tx.send(true);
                    }
                }
                DownloadStatus::Pausing
                | DownloadStatus::Paused
                | DownloadStatus::Failed
                | DownloadStatus::Cancelled => {}
            }
            task.to_summary()
        };
        self.persist();
        Ok(summary)
    }

    /// Put a paused or failed download back in the queue; it starts when a slot is free.
    pub fn resume_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let task_lock = self.get_task(id)?;
        {
            let mut task = task_lock.lock().unwrap();
            match task.status {
                DownloadStatus::Completed => return Err(RpcError::already_completed(id)),
                DownloadStatus::Cancelled => {
                    return Err(RpcError::invalid_request(Some(
                        "Cannot resume cancelled download".into(),
                    )))
                }
                DownloadStatus::Pausing => {
                    return Err(RpcError::operation_in_progress(id, "pausing"))
                }
                DownloadStatus::Active | DownloadStatus::Queued => {}
                DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Retrying => {
                    task.status = DownloadStatus::Queued;
                    task.error_message = None;
                    task.resume_after_restart = false;
                }
            }
        }

        self.persist();
        self.schedule();
        let summary = task_lock.lock().unwrap().to_summary();
        Ok(summary)
    }

    pub fn cancel_download(
        &self,
        params: CancelDownloadParams,
    ) -> Result<DownloadSummary, RpcError> {
        let task_lock = self.get_task(&params.id)?;
        let summary = {
            let mut task = task_lock.lock().unwrap();
            if task.status == DownloadStatus::Completed {
                return Err(RpcError::already_completed(&params.id));
            }
            if task.status != DownloadStatus::Cancelled {
                task.status = DownloadStatus::Cancelled;
                task.resume_after_restart = false;
                if let Some(ref tx) = task.cancel_tx {
                    let _ = tx.send(true);
                }
            }
            if params.delete_partial {
                if task.running {
                    // The engine still has the files open; delete them once it stops.
                    task.delete_on_stop = true;
                } else {
                    task.delete_partial_files();
                }
            }
            task.to_summary()
        };
        self.persist();
        Ok(summary)
    }

    /// Forget a download that is not running. Its files are left alone.
    pub fn remove_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let summary = {
            let mut state = self.state.lock().unwrap();
            let summary = {
                let task = state
                    .downloads
                    .get(id)
                    .ok_or_else(|| RpcError::download_not_found(id))?
                    .lock()
                    .unwrap();
                if task.running {
                    return Err(RpcError::operation_in_progress(id, "download"));
                }
                task.to_summary()
            };
            state.downloads.remove(id);
            summary
        };
        self.persist();
        Ok(summary)
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
        let summary = self.get_task(id)?.lock().unwrap().to_summary();
        Ok(summary)
    }

    pub fn limits(&self) -> DaemonLimits {
        self.state.lock().unwrap().limits.clone()
    }

    /// Change the queue limits. A lower `max_active` lets running downloads finish
    /// rather than pausing them.
    pub fn set_limits(&self, params: SetLimitsParams) -> Result<DaemonLimits, RpcError> {
        if let Some(n) = params.max_active {
            if !(1..=MAX_ACTIVE_LIMIT).contains(&n) {
                return Err(RpcError::invalid_params(format!(
                    "max_active must be between 1 and {MAX_ACTIVE_LIMIT}"
                )));
            }
        }
        if let Some(bps) = params.max_bytes_per_sec {
            if bps != 0 && bps < MIN_BYTES_PER_SEC {
                return Err(RpcError::invalid_params(format!(
                    "max_bytes_per_sec must be 0 (unlimited) or at least {MIN_BYTES_PER_SEC}"
                )));
            }
        }

        let limits = {
            let mut state = self.state.lock().unwrap();
            if let Some(n) = params.max_active {
                state.limits.max_active = n;
            }
            if let Some(bps) = params.max_bytes_per_sec {
                state.limits.max_bytes_per_sec = bps;
            }
            state.limits.clone()
        };
        self.rate_limiter
            .set_bytes_per_sec(limits.max_bytes_per_sec);

        self.persist();
        self.schedule();
        Ok(limits)
    }

    /// Start queued downloads, oldest first, until `max_active` are running.
    pub fn schedule(&self) {
        let state = self.state.lock().unwrap();
        if state.is_shutting_down {
            return;
        }

        let mut running = 0;
        let mut queued = Vec::new();
        for (id, task_lock) in &state.downloads {
            let task = task_lock.lock().unwrap();
            if task.running {
                running += 1;
            } else if task.status == DownloadStatus::Queued {
                queued.push((download_id_number(id), Arc::clone(task_lock)));
            }
        }
        queued.sort_by_key(|(n, _)| *n);

        let free = state.limits.max_active.saturating_sub(running);
        for (_, task_lock) in queued.into_iter().take(free) {
            self.start_task(&task_lock);
        }
    }

    /// Spawn the engine for a queued task: a resume when its state file exists,
    /// otherwise a fresh download. Called by `schedule` with the state lock held.
    fn start_task(&self, task_lock: &Arc<Mutex<TaskRecord>>) {
        let mut task = task_lock.lock().unwrap();
        if task.running || task.status != DownloadStatus::Queued {
            return;
        }

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let progress = Arc::new(Progress::with_rate_limiter(Arc::clone(&self.rate_limiter)));
        let resume_from = task.state_file().filter(|p| p.exists());
        if resume_from.is_none() {
            task.output = None;
            task.downloaded_bytes = 0;
        }

        task.status = DownloadStatus::Active;
        task.running = true;
        task.cancel_tx = Some(cancel_tx);
        task.error_message = None;
        task.progress = Arc::clone(&progress);
        task.last_tick_bytes = task.downloaded_bytes;
        task.last_tick_time = Instant::now();

        let template = match task.requested_name {
            Some(ref name) => format!("{{downloads}}/{}", name),
            None => "{downloads}/{name}".to_string(),
        };
        let url = task.url.clone();
        let directory = task.directory.clone();
        let connections = task.connections;
        let core = self.clone();
        let record = Arc::clone(task_lock);

        tokio::spawn(async move {
            let outcome = match resume_from {
                Some(state_file) => {
                    engine::resume_with_options(
                        &core.client,
                        &state_file,
                        None,
                        Some(progress),
                        Some(cancel_rx),
                    )
                    .await
                }
                None => match Url::parse(&url) {
                    Ok(url) => {
                        let req = DownloadRequest {
                            url,
                            template,
                            downloads_dir: directory,
                            connections,
                            progress,
                            cancel: Some(cancel_rx),
                        };
                        engine::download(&core.client, req).await
                    }
                    Err(e) => Err(Error::Protocol(format!("invalid URL: {e}"))),
                },
            };
            core.finish_task(&record, outcome);
        });
    }

    fn finish_task(&self, record: &Arc<Mutex<TaskRecord>>, outcome: Result<Outcome, Error>) {
        {
            let mut rec = record.lock().unwrap();
            rec.apply_outcome(outcome);
            if rec.delete_on_stop {
                rec.delete_partial_files();
                rec.delete_on_stop = false;
            }
        }
        self.persist();
        self.schedule();
    }

    pub fn tick_telemetry(&self) {
        let mut changed = false;
        {
            let state = self.state.lock().unwrap();
            let now = Instant::now();

            for task_lock in state.downloads.values() {
                let mut task = task_lock.lock().unwrap();
                if task.status != DownloadStatus::Active {
                    continue;
                }
                changed |= task.sync_from_progress();

                let current_bytes = task.progress.downloaded.load(Ordering::Relaxed);
                let elapsed = now.duration_since(task.last_tick_time).as_secs_f64();
                if elapsed < 0.1 {
                    continue;
                }

                let delta = current_bytes.saturating_sub(task.last_tick_bytes);
                let speed = (delta as f64 / elapsed) as u64;
                task.speed_bps = speed;
                task.downloaded_bytes = current_bytes;
                task.last_tick_bytes = current_bytes;
                task.last_tick_time = now;

                if let Some(total) = task.total_bytes {
                    if speed > 0 && total > current_bytes {
                        task.eta_secs = Some((total - current_bytes) / speed);
                    } else if total <= current_bytes {
                        task.eta_secs = Some(0);
                    }
                }

                let segments = task
                    .progress
                    .completed
                    .lock()
                    .unwrap()
                    .ranges()
                    .iter()
                    .map(|r| SegmentProgress {
                        start: r.start,
                        end: r.end,
                        downloaded_bytes: r.len(),
                        state: "completed".to_string(),
                    })
                    .collect();

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

        if changed {
            self.persist();
        }
    }

    fn snapshot(&self) -> QueueSnapshot {
        let state = self.state.lock().unwrap();
        let mut downloads: Vec<_> = state
            .downloads
            .values()
            .map(|t| t.lock().unwrap().to_saved())
            .collect();
        downloads.sort_by_key(|d| download_id_number(&d.id));
        QueueSnapshot {
            version: QUEUE_VERSION,
            next_id: state.next_id,
            limits: state.limits.clone(),
            downloads,
        }
    }

    /// Save the queue to disk if this core has a store. Failures are logged; the
    /// daemon keeps running on its in-memory queue.
    fn persist(&self) {
        let Some(ref store) = self.store else {
            return;
        };
        if let Err(e) = store.save(&self.snapshot()) {
            tracing::warn!(
                "failed to save download queue to {}: {}",
                store.path().display(),
                e
            );
        }
    }
}
