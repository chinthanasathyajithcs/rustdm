use crate::daemon::folder_check::{self, FolderProblem};
use crate::daemon::queue_store::{QueueSnapshot, QueueStore, QUEUE_VERSION};
use crate::daemon::secrets::SecretStore;
use crate::daemon::task::TaskRecord;
use crate::daemon::{defaults, fair_share, queue_order, sign_in};
use crate::engine::{self, build_client, DownloadRequest, Outcome, Progress};
use crate::error::Error;
use crate::ipc::protocol::*;
use std::collections::HashMap;
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
    /// Every download id in queue order. Queued downloads start in this order.
    pub order: Vec<String>,
    pub next_id: u64,
    pub start_time: Instant,
    pub is_shutting_down: bool,
    pub limits: DaemonLimits,
    /// Defaults for new downloads, as saved.
    pub defaults: SavedDefaults,
}

impl DaemonState {
    /// Every download's record, in queue order.
    pub fn in_order(&self) -> impl Iterator<Item = &Arc<Mutex<TaskRecord>>> {
        self.order.iter().filter_map(|id| self.downloads.get(id))
    }
}

/// Give each running download its fair share of the overall speed cap.
/// Called whenever downloads start or stop, and on every telemetry tick.
fn rebalance_shares(state: &DaemonState, now: Instant) {
    let running: Vec<_> = state
        .in_order()
        .filter(|t| t.lock().unwrap().running)
        .collect();
    let demands: Vec<_> = running
        .iter()
        .map(|t| {
            let t = t.lock().unwrap();
            fair_share::demand(
                t.speed_bps,
                t.share_limiter.bytes_per_sec(),
                t.max_bytes_per_sec(),
                t.speed_is_settled(now),
            )
        })
        .collect();
    let shares = fair_share::fair_shares(state.limits.max_bytes_per_sec, &demands);
    for (task, share) in running.iter().zip(shares) {
        task.lock().unwrap().share_limiter.set_bytes_per_sec(share);
    }
}

/// A download's folder must be usable before it is queued, so a bad one is
/// reported to the client (under the Add dialog's Folder field) instead of
/// failing later in the table.
fn check_download_folder(folder: &std::path::Path) -> Result<(), RpcError> {
    folder_check::check_folder(folder).map_err(|problem| match problem {
        FolderProblem::CantWrite(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            RpcError::permission_denied(folder)
        }
        other => RpcError::invalid_path(folder, &other.to_string()),
    })
}

/// A speed cap must be 0 (none) or at least `MIN_BYTES_PER_SEC`.
/// What a client asked for when it added a link, once the defaults are filled in.
struct NewDownload {
    url: String,
    directory: std::path::PathBuf,
    filename: Option<String>,
    connections: u32,
    max_bytes_per_sec: u64,
}

/// An add of `url` is turned away while the daemon shuts down, or while that
/// link is already queued or downloading.
fn refuse_unless_open(state: &DaemonState, url: &str) -> Result<(), RpcError> {
    if state.is_shutting_down {
        return Err(RpcError::daemon_shutting_down());
    }
    for task_lock in state.downloads.values() {
        let task = task_lock.lock().unwrap();
        let unfinished = matches!(
            task.status,
            DownloadStatus::Active | DownloadStatus::Queued | DownloadStatus::Pausing
        );
        if task.url == url && unfinished {
            return Err(RpcError::duplicate_download(url, &task.id));
        }
    }
    Ok(())
}

fn validate_speed(bytes_per_sec: u64) -> Result<(), RpcError> {
    if bytes_per_sec != 0 && bytes_per_sec < MIN_BYTES_PER_SEC {
        return Err(RpcError::invalid_params(format!(
            "max_bytes_per_sec must be 0 (unlimited) or at least {MIN_BYTES_PER_SEC}"
        )));
    }
    Ok(())
}

/// Owns every download and the queue rules. Lock order: `state`, then a task.
/// `persist` and `schedule` take the locks themselves, so call them with none held.
#[derive(Clone)]
pub struct DaemonCore {
    pub state: Arc<Mutex<DaemonState>>,
    pub client: reqwest::Client,
    pub telemetry_tx: broadcast::Sender<TelemetryTick>,
    pub shutdown_notify: Arc<tokio::sync::Notify>,
    store: Option<Arc<QueueStore>>,
    /// Folder holding the queue file and the log, when the queue is saved.
    data_dir: Option<std::path::PathBuf>,
    /// Browser cookies of downloads that came with them, when the queue is saved.
    pub(super) secrets: Option<Arc<SecretStore>>,
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

        let defaults = snapshot
            .as_ref()
            .map(|s| s.defaults.clone())
            .unwrap_or_default();

        let data_dir = store
            .as_ref()
            .and_then(|s| s.path().parent())
            .map(|p| p.to_path_buf());
        let secrets = data_dir
            .as_deref()
            .map(|d| Arc::new(SecretStore::in_dir(d)));

        let mut downloads = HashMap::new();
        let mut order = Vec::new();
        let mut next_id = 0;
        let mut restored_changes = false;
        if let Some(snapshot) = snapshot {
            next_id = snapshot.next_id;
            // Saved in queue order.
            for saved in snapshot.downloads {
                next_id = next_id.max(download_id_number(&saved.id));
                let mut rec = TaskRecord::from_saved(saved);
                restored_changes |= sign_in::restore(secrets.as_deref(), &mut rec);
                order.push(rec.id.clone());
                downloads.insert(rec.id.clone(), Arc::new(Mutex::new(rec)));
            }
        }
        if let Some(secrets) = &secrets {
            let records: Vec<_> = downloads.values().map(|t| t.lock().unwrap()).collect();
            let records: Vec<&TaskRecord> = records.iter().map(|r| &**r).collect();
            sign_in::sweep(secrets, &records);
        }

        let (telemetry_tx, _) = broadcast::channel(1024);
        let core = Self {
            state: Arc::new(Mutex::new(DaemonState {
                downloads,
                order,
                next_id,
                start_time: Instant::now(),
                is_shutting_down: false,
                limits,
                defaults,
            })),
            client: build_client(),
            telemetry_tx,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            data_dir,
            secrets,
            store: store.map(Arc::new),
        };
        if restored_changes {
            core.persist();
        }
        core
    }

    /// Folder holding the queue file and the log, if this core saves its queue.
    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.data_dir.as_deref()
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
        // Every field the client left out comes from the defaults.
        let defaults = self.get_defaults();
        let max_bytes_per_sec = params
            .max_bytes_per_sec
            .unwrap_or(defaults.max_bytes_per_sec);
        validate_speed(max_bytes_per_sec)?;
        if let Some(name) = &params.filename {
            if crate::template::sanitize_component(name).is_empty() {
                return Err(RpcError::invalid_params(format!(
                    "{name:?} is not a usable file name"
                )));
            }
        }

        let asked_folder = params.destination.clone();
        let directory = params.destination.unwrap_or(defaults.folder);
        check_download_folder(&directory)?;
        let validated = params.context.as_ref().map(sign_in::validate).transpose()?;
        // Turn the add away before paying for a client when it will be turned away anyway.
        self.refuse_unless_open_for(&params.url)?;
        let context = validated.map(sign_in::with_client).transpose()?;

        let task = {
            let mut state = self.state.lock().unwrap();
            refuse_unless_open(&state, &params.url)?;

            // Started again from the browser, with a fresh sign-in: carry on
            // with the stopped download, from its partial file. Anything less
            // (no cookies, another folder or name) is a different download.
            let ask = sign_in::AddRequest {
                url: &params.url,
                folder: asked_folder.as_deref(),
                file_name: params.filename.as_deref(),
            };
            let continuing = context
                .as_ref()
                .filter(|c| !c.context.cookies.is_empty())
                .and_then(|c| Some((sign_in::stopped_download_of(&state, &ask)?, c)));
            match continuing {
                Some((stopped, checked)) => self.continue_stopped(&stopped, checked),
                None => {
                    let connections = params
                        .connections
                        .unwrap_or(defaults.connections as usize)
                        .clamp(1, MAX_CONNECTIONS) as u32;
                    self.insert_new_download(
                        &mut state,
                        NewDownload {
                            url: params.url,
                            directory,
                            filename: params.filename,
                            connections,
                            max_bytes_per_sec,
                        },
                        context.as_ref(),
                    )
                }
            }
        };
        self.persist();
        self.schedule();
        // After scheduling, so the summary shows a download that started.
        let summary = task.lock().unwrap().to_summary();
        Ok(summary)
    }

    /// Refuse an add of `url` while the daemon is shutting down or already
    /// downloads it.
    fn refuse_unless_open_for(&self, url: &str) -> Result<(), RpcError> {
        refuse_unless_open(&self.state.lock().unwrap(), url)
    }

    /// Carry on a stopped download with the fresh sign-in the browser brought.
    /// Called with the state lock held, so nothing changed it since it was chosen.
    fn continue_stopped(
        &self,
        stopped: &Arc<Mutex<TaskRecord>>,
        checked: &sign_in::CheckedContext,
    ) -> Arc<Mutex<TaskRecord>> {
        {
            let mut task = stopped.lock().unwrap();
            task.set_context(&checked.context, checked.client.clone());
            task.requeue();
            // Saved under the task's lock, so a cancel cannot slip in between.
            self.keep_secret(&task, &checked.context.cookies);
            sign_in::log_context(&task.id, &checked.context, "continued with");
        }
        Arc::clone(stopped)
    }

    /// Put a new download in the queue. Called with the state lock held.
    fn insert_new_download(
        &self,
        state: &mut DaemonState,
        new: NewDownload,
        context: Option<&sign_in::CheckedContext>,
    ) -> Arc<Mutex<TaskRecord>> {
        state.next_id += 1;
        let id = format!("dl_{}", state.next_id);
        let mut task = TaskRecord::new(
            id.clone(),
            new.url,
            new.directory,
            new.filename,
            new.connections,
        );
        task.speed_limiter.set_bytes_per_sec(new.max_bytes_per_sec);
        if let Some(checked) = context {
            task.set_context(&checked.context, checked.client.clone());
            self.keep_secret(&task, &checked.context.cookies);
            sign_in::log_context(&id, &checked.context, "added with");
        }
        let task = Arc::new(Mutex::new(task));
        state.order.push(id.clone());
        state.downloads.insert(id, Arc::clone(&task));
        task
    }

    pub fn pause_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let task_lock = self.get_task(id)?;
        let summary = {
            let mut task = task_lock.lock().unwrap();
            match task.status {
                DownloadStatus::Completed => return Err(RpcError::cannot_pause_completed(id)),
                DownloadStatus::Queued => task.status = DownloadStatus::Paused,
                DownloadStatus::Retrying if !task.running => {
                    task.status = DownloadStatus::Paused;
                    task.retry_at = None;
                }
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

    /// Put a paused, failed, or cancelled download back in the queue; it starts
    /// when a slot is free.
    pub fn resume_download(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let task_lock = self.get_task(id)?;
        {
            let mut task = task_lock.lock().unwrap();
            match task.status {
                DownloadStatus::Completed => return Err(RpcError::already_completed(id)),
                DownloadStatus::Cancelled if task.running => {
                    return Err(RpcError::operation_in_progress(id, "cancelling"))
                }
                DownloadStatus::Pausing => {
                    return Err(RpcError::operation_in_progress(id, "pausing"))
                }
                DownloadStatus::Active | DownloadStatus::Queued => {}
                // A stopped cancel continues from its partial file, or starts
                // over if `--delete` removed it.
                DownloadStatus::Paused
                | DownloadStatus::Failed
                | DownloadStatus::Retrying
                | DownloadStatus::Cancelled => task.requeue(),
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
                if task.drop_cookies() {
                    self.delete_secret(&task.id);
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
        self.refresh_client(&task_lock);
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
                // Saves happen under the state lock too, so none is on its way.
                self.delete_secret(id);
                task.to_summary()
            };
            state.downloads.remove(id);
            state.order.retain(|x| x != id);
            summary
        };
        self.persist();
        Ok(summary)
    }

    /// Every download, in queue order.
    pub fn list_downloads(&self) -> Vec<DownloadSummary> {
        let state = self.state.lock().unwrap();
        state
            .in_order()
            .map(|t| t.lock().unwrap().to_summary())
            .collect()
    }

    /// Move a download within the queue. Running downloads keep running; the new
    /// order decides which queued download starts next.
    pub fn move_download(&self, params: MoveDownloadParams) -> Result<DownloadSummary, RpcError> {
        let task_lock = {
            let mut state = self.state.lock().unwrap();
            let order = queue_order::moved(&state.order, &params.id, params.position)
                .ok_or_else(|| RpcError::download_not_found(&params.id))?;
            state.order = order;
            state
                .downloads
                .get(&params.id)
                .cloned()
                .ok_or_else(|| RpcError::download_not_found(&params.id))?
        };
        self.persist();
        let summary = task_lock.lock().unwrap().to_summary();
        Ok(summary)
    }

    /// Cap one download's speed, or remove its cap with 0. Takes effect at once,
    /// including for a download that is running.
    pub fn set_download_speed(
        &self,
        params: SetDownloadSpeedParams,
    ) -> Result<DownloadSummary, RpcError> {
        validate_speed(params.max_bytes_per_sec)?;
        let task_lock = self.get_task(&params.id)?;
        let summary = {
            let task = task_lock.lock().unwrap();
            task.speed_limiter
                .set_bytes_per_sec(params.max_bytes_per_sec);
            task.to_summary()
        };
        self.persist();
        Ok(summary)
    }

    pub fn get_status(&self, id: &str) -> Result<DownloadSummary, RpcError> {
        let summary = self.get_task(id)?.lock().unwrap().to_summary();
        Ok(summary)
    }

    pub fn limits(&self) -> DaemonLimits {
        self.state.lock().unwrap().limits.clone()
    }

    /// Defaults for new downloads, with the built-in values filled in.
    pub fn get_defaults(&self) -> DownloadDefaults {
        defaults::resolve(&self.state.lock().unwrap().defaults)
    }

    /// Change the defaults for new downloads; fields left out are kept.
    pub fn set_defaults(&self, params: SetDefaultsParams) -> Result<DownloadDefaults, RpcError> {
        defaults::check_folder(&params)?;
        let resolved = {
            let mut state = self.state.lock().unwrap();
            state.defaults = defaults::updated(&state.defaults, params, validate_speed)?;
            defaults::resolve(&state.defaults)
        };
        self.persist();
        Ok(resolved)
    }

    /// Change the queue limits. When `max_active` drops below the number running,
    /// the downloads furthest down the queue are stopped and queued again; they
    /// resume from their state files once a slot is free.
    pub fn set_limits(&self, params: SetLimitsParams) -> Result<DaemonLimits, RpcError> {
        if let Some(n) = params.max_active {
            if !(1..=MAX_ACTIVE_LIMIT).contains(&n) {
                return Err(RpcError::invalid_params(format!(
                    "max_active must be between 1 and {MAX_ACTIVE_LIMIT}"
                )));
            }
        }
        if let Some(bps) = params.max_bytes_per_sec {
            validate_speed(bps)?;
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
        self.requeue_extra_downloads();
        self.persist();
        self.schedule();
        Ok(limits)
    }

    /// Stop the downloads beyond `max_active`, furthest down the queue first, and
    /// put them back in the queue. Their engines save state as they stop.
    fn requeue_extra_downloads(&self) {
        let state = self.state.lock().unwrap();
        let mut downloading = 0;
        for task_lock in state.in_order() {
            let mut task = task_lock.lock().unwrap();
            let downloading_now = task.running
                && matches!(
                    task.status,
                    DownloadStatus::Active | DownloadStatus::Retrying
                );
            if !downloading_now {
                continue;
            }
            downloading += 1;
            if downloading <= state.limits.max_active {
                continue;
            }
            task.status = DownloadStatus::Queued;
            if let Some(ref tx) = task.cancel_tx {
                let _ = tx.send(true);
            }
        }
    }

    /// Start queued downloads in queue order until `max_active` are running.
    /// Downloads still stopping count as running, since they hold connections.
    pub fn schedule(&self) {
        let state = self.state.lock().unwrap();
        if state.is_shutting_down {
            return;
        }

        let mut running = 0;
        let mut queued = Vec::new();
        for task_lock in state.in_order() {
            let task = task_lock.lock().unwrap();
            if task.running {
                running += 1;
            } else if task.status == DownloadStatus::Queued && !task.client_pending {
                queued.push(Arc::clone(task_lock));
            }
        }

        let free = state.limits.max_active.saturating_sub(running);
        for task_lock in queued.into_iter().take(free) {
            self.start_task(&task_lock);
        }
        rebalance_shares(&state, Instant::now());
    }

    /// Spawn the engine for a queued task: a resume when its state file exists,
    /// otherwise a fresh download. Called by `schedule` with the state lock held.
    fn start_task(&self, task_lock: &Arc<Mutex<TaskRecord>>) {
        let mut task = task_lock.lock().unwrap();
        // One that waits for its client starts when `refresh_client` has built it.
        if task.running || task.status != DownloadStatus::Queued || task.client_pending {
            return;
        }

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let progress = Arc::new(Progress::with_rate_limiters(
            Arc::clone(&task.share_limiter),
            Arc::clone(&task.speed_limiter),
        ));
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
        task.begin_run();

        // The requested name is a value for {name}, never part of the template,
        // so it is sanitized and cannot reach outside the folder.
        let template = "{downloads}/{name}".to_string();
        let file_name = task.requested_name.clone();
        let url = task.url.clone();
        let directory = task.directory.clone();
        let connections = task.connections;
        let client = task.client.clone().unwrap_or_else(|| self.client.clone());
        let core = self.clone();
        let record = Arc::clone(task_lock);

        tokio::spawn(async move {
            let outcome = match resume_from {
                Some(state_file) => {
                    engine::resume_with_options(
                        &client,
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
                            file_name,
                            downloads_dir: directory,
                            connections,
                            progress,
                            cancel: Some(cancel_rx),
                        };
                        engine::download(&client, req).await
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
            let had_cookies = rec.has_cookies;
            rec.apply_outcome(outcome);
            if rec.delete_on_stop {
                rec.delete_partial_files();
                rec.delete_on_stop = false;
            }
            // Done, or turned down by the site: the cookies are of no more use.
            if had_cookies && !rec.has_cookies {
                self.delete_secret(&rec.id);
            }
        }
        self.refresh_client(record);
        self.persist();
        self.schedule();
    }

    /// Put downloads whose retry wait is over back in the queue.
    pub fn retry_due_downloads(&self) {
        let now = Instant::now();
        let mut any = false;
        {
            let state = self.state.lock().unwrap();
            if state.is_shutting_down {
                return;
            }
            for task_lock in state.downloads.values() {
                let mut task = task_lock.lock().unwrap();
                if task.retry_is_due(now) {
                    task.status = DownloadStatus::Queued;
                    task.retry_at = None;
                    any = true;
                }
            }
        }
        if any {
            self.persist();
            self.schedule();
        }
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
                let current_bytes = task.measure(now);
                let speed = task.speed_bps;

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

                let lanes: Vec<SegmentProgress> = task
                    .progress
                    .lane_snapshots()
                    .into_iter()
                    .map(|lane| SegmentProgress {
                        start: lane.start,
                        end: lane.end,
                        downloaded_bytes: lane.cursor.saturating_sub(lane.start),
                        state: if lane.cursor < lane.end {
                            "active"
                        } else {
                            "done"
                        }
                        .to_string(),
                    })
                    .collect();
                let active_connections = if lanes.is_empty() {
                    task.connections as usize
                } else {
                    lanes.iter().filter(|l| l.state == "active").count()
                };

                let tick = TelemetryTick {
                    id: task.id.clone(),
                    downloaded_bytes: current_bytes,
                    total_bytes: task.total_bytes,
                    speed_bps: speed,
                    active_connections,
                    segments,
                    lanes,
                    stall: task.stall(now),
                };
                let _ = self.telemetry_tx.send(tick);
            }
            rebalance_shares(&state, now);
        }

        if changed {
            self.persist();
        }
    }

    fn snapshot(&self) -> QueueSnapshot {
        let state = self.state.lock().unwrap();
        let downloads = state
            .in_order()
            .map(|t| t.lock().unwrap().to_saved())
            .collect();
        QueueSnapshot {
            version: QUEUE_VERSION,
            next_id: state.next_id,
            limits: state.limits.clone(),
            defaults: state.defaults.clone(),
            downloads,
        }
    }

    /// Save the queue to disk if this core has a store. Failures are logged; the
    /// daemon keeps running on its in-memory queue.
    pub(super) fn persist(&self) {
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
