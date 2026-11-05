use crate::daemon::queue_store::SavedDownload;
use crate::engine::{Outcome, PauseReason, Progress};
use crate::error::Error;
use crate::ipc::protocol::{DownloadStatus, DownloadSummary};
use crate::limiter::RateLimiter;
use crate::state::{part_path, state_path};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

/// In-memory representation of a tracked download task.
pub struct TaskRecord {
    pub id: String,
    pub url: String,
    /// Folder the download saves into.
    pub directory: PathBuf,
    /// File name the user asked for, if any.
    pub requested_name: Option<String>,
    pub filename: String,
    /// Final file path once the engine has chosen it.
    pub output: Option<PathBuf>,
    pub status: DownloadStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub progress: Arc<Progress>,
    pub cancel_tx: Option<tokio::sync::watch::Sender<bool>>,
    pub error_message: Option<String>,
    pub last_tick_bytes: u64,
    pub last_tick_time: Instant,
    pub connections: u32,
    /// An engine task is running for this download.
    pub running: bool,
    /// Paused by daemon shutdown rather than by the user, so it is queued again on restart.
    pub resume_after_restart: bool,
    /// Delete the partial files once the engine has stopped.
    pub delete_on_stop: bool,
}

impl TaskRecord {
    pub fn new(
        id: String,
        url: String,
        directory: PathBuf,
        requested_name: Option<String>,
        connections: u32,
        limiter: &Arc<RateLimiter>,
    ) -> Self {
        let filename = requested_name
            .clone()
            .unwrap_or_else(|| placeholder_name(&url));
        Self {
            id,
            url,
            directory,
            requested_name,
            filename,
            output: None,
            status: DownloadStatus::Queued,
            total_bytes: None,
            downloaded_bytes: 0,
            speed_bps: 0,
            eta_secs: None,
            progress: Arc::new(Progress::with_rate_limiter(Arc::clone(limiter))),
            cancel_tx: None,
            error_message: None,
            last_tick_bytes: 0,
            last_tick_time: Instant::now(),
            connections,
            running: false,
            resume_after_restart: false,
            delete_on_stop: false,
        }
    }

    pub fn from_saved(saved: SavedDownload, limiter: &Arc<RateLimiter>) -> Self {
        let mut rec = Self::new(
            saved.id,
            saved.url,
            saved.directory,
            saved.requested_name,
            saved.connections,
            limiter,
        );
        rec.filename = saved.filename;
        rec.output = saved.output;
        rec.total_bytes = saved.total_bytes;
        rec.downloaded_bytes = saved.downloaded_bytes;
        rec.error_message = saved.error_message;
        // Nothing runs right after a restart; anything that was running waits in the queue.
        rec.status = match saved.status {
            DownloadStatus::Active | DownloadStatus::Retrying | DownloadStatus::Pausing => {
                DownloadStatus::Queued
            }
            other => other,
        };
        rec
    }

    pub fn to_saved(&self) -> SavedDownload {
        SavedDownload {
            id: self.id.clone(),
            url: self.url.clone(),
            directory: self.directory.clone(),
            requested_name: self.requested_name.clone(),
            filename: self.filename.clone(),
            output: self.output.clone(),
            connections: self.connections,
            status: self.status_to_save(),
            total_bytes: self.total_bytes,
            downloaded_bytes: self.downloaded_bytes,
            error_message: self.error_message.clone(),
        }
    }

    /// The status a restarted daemon should give this download.
    fn status_to_save(&self) -> DownloadStatus {
        match self.status {
            DownloadStatus::Active | DownloadStatus::Retrying => DownloadStatus::Queued,
            DownloadStatus::Pausing | DownloadStatus::Paused if self.resume_after_restart => {
                DownloadStatus::Queued
            }
            DownloadStatus::Pausing => DownloadStatus::Paused,
            other => other,
        }
    }

    pub fn to_summary(&self) -> DownloadSummary {
        DownloadSummary {
            id: self.id.clone(),
            url: self.url.clone(),
            destination: self
                .output
                .clone()
                .unwrap_or_else(|| self.directory.clone()),
            filename: self.filename.clone(),
            status: self.status,
            downloaded_bytes: self.downloaded_bytes,
            total_bytes: self.total_bytes,
            speed_bps: self.speed_bps,
            eta_secs: self.eta_secs,
            error_message: self.error_message.clone(),
        }
    }

    /// The `.part.rdm` file a resume would start from, if the output is known.
    pub fn state_file(&self) -> Option<PathBuf> {
        self.output.as_deref().map(state_path)
    }

    /// Copy the total size and output path from the engine once it knows them.
    /// Returns true if anything changed.
    pub fn sync_from_progress(&mut self) -> bool {
        let mut changed = false;
        if self.total_bytes.is_none() {
            let total = self.progress.total.load(Ordering::Relaxed);
            if total > 0 {
                self.total_bytes = Some(total);
                changed = true;
            }
        }
        if self.output.is_none() {
            let output = self.progress.output.lock().unwrap().clone();
            if let Some(path) = output {
                if let Some(name) = path.file_name() {
                    self.filename = name.to_string_lossy().to_string();
                }
                self.output = Some(path);
                changed = true;
            }
        }
        changed
    }

    pub fn delete_partial_files(&self) {
        if let Some(output) = &self.output {
            let _ = std::fs::remove_file(part_path(output));
            let _ = std::fs::remove_file(state_path(output));
        }
    }

    /// Record how an engine run ended. A cancelled task stays cancelled: the engine
    /// reports a cancel as a pause, but the user asked for the download to stop.
    pub fn apply_outcome(&mut self, outcome: Result<Outcome, Error>) {
        self.sync_from_progress();
        self.downloaded_bytes = self.progress.downloaded.load(Ordering::Relaxed);
        self.speed_bps = 0;
        self.eta_secs = None;
        self.cancel_tx = None;
        self.running = false;
        let cancelled = self.status == DownloadStatus::Cancelled;

        match outcome {
            Ok(Outcome::Completed { path, steals: _ }) => {
                self.status = DownloadStatus::Completed;
                if let Some(name) = path.file_name() {
                    self.filename = name.to_string_lossy().to_string();
                }
                self.output = Some(path);
                self.total_bytes = Some(self.downloaded_bytes);
                self.eta_secs = Some(0);
                self.error_message = None;
            }
            Ok(Outcome::Paused { reason, state: _ }) => {
                if !cancelled {
                    self.status = DownloadStatus::Paused;
                }
                if let PauseReason::LinkExpired { status } = reason {
                    self.error_message = Some(format!("link expired or removed (HTTP {status})"));
                }
            }
            Err(e) => {
                if !cancelled {
                    self.status = DownloadStatus::Failed;
                }
                self.error_message = Some(e.to_string());
            }
        }
    }
}

/// A readable name from the URL's last path segment, shown until the engine
/// picks the real file name.
fn placeholder_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()?
                .rfind(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "download".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: DownloadStatus) -> TaskRecord {
        let mut rec = TaskRecord::new(
            "dl_1".to_string(),
            "http://example.com/files/ubuntu.iso".to_string(),
            PathBuf::from("/downloads"),
            None,
            8,
            &Arc::new(RateLimiter::new(0)),
        );
        rec.status = status;
        rec
    }

    #[test]
    fn placeholder_name_comes_from_the_url() {
        assert_eq!(record(DownloadStatus::Queued).filename, "ubuntu.iso");
        assert_eq!(placeholder_name("http://example.com/"), "download");
    }

    #[test]
    fn running_downloads_are_saved_as_queued() {
        assert_eq!(
            record(DownloadStatus::Active).to_saved().status,
            DownloadStatus::Queued
        );
    }

    #[test]
    fn user_pauses_survive_a_restart_but_shutdown_pauses_do_not() {
        let user_paused = record(DownloadStatus::Pausing);
        assert_eq!(user_paused.to_saved().status, DownloadStatus::Paused);

        let mut shutdown_paused = record(DownloadStatus::Paused);
        shutdown_paused.resume_after_restart = true;
        assert_eq!(shutdown_paused.to_saved().status, DownloadStatus::Queued);
    }

    #[test]
    fn cancelled_stays_cancelled_when_the_engine_reports_a_pause() {
        let mut rec = record(DownloadStatus::Cancelled);
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::Cancelled,
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert_eq!(rec.status, DownloadStatus::Cancelled);
    }
}
