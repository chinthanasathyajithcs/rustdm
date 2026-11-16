use crate::daemon::queue_store::SavedDownload;
use crate::daemon::speed::SpeedMeter;
use crate::engine::{Outcome, PauseReason, Progress};
use crate::error::{
    is_sign_in_status, link_status_message, sign_in_expired_message, sign_in_page_message, Error,
};
use crate::ipc::protocol::{DownloadStatus, DownloadSummary};
use crate::limiter::RateLimiter;
use crate::request_context::RequestContext;
use crate::state::{part_path, state_path};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Automatic retries in a row, without progress in between, before a download fails.
pub const MAX_AUTO_RETRIES: u32 = 5;
/// Wait before the first automatic retry; each later one waits twice as long.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(3);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(120);

/// Wait before automatic retry number `done + 1`.
pub fn auto_retry_delay(done: u32) -> Duration {
    FIRST_RETRY_DELAY
        .saturating_mul(1 << done.min(16))
        .min(MAX_RETRY_DELAY)
}

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
    /// Speed averaged over the last few seconds.
    pub speed: SpeedMeter,
    /// Bytes on disk when the current engine run started.
    pub bytes_at_start: u64,
    /// Automatic retries since the download last made progress.
    pub retries: u32,
    /// When a `Retrying` download goes back into the queue.
    pub retry_at: Option<Instant>,
    pub connections: u32,
    /// This download's own speed cap; a rate of 0 means none.
    pub speed_limiter: Arc<RateLimiter>,
    /// This download's share of the overall speed cap, set by the daemon
    /// (see `daemon::fair_share`); a rate of 0 means no cap.
    pub share_limiter: Arc<RateLimiter>,
    /// When the current engine run started.
    pub run_started: Instant,
    /// When new data last arrived, or the current run started.
    pub moved_at: Instant,
    /// An engine task is running for this download.
    pub running: bool,
    /// Paused by daemon shutdown rather than by the user, so it is queued again on restart.
    pub resume_after_restart: bool,
    /// Delete the partial files once the engine has stopped.
    pub delete_on_stop: bool,
    /// The download came with browser cookies, kept in the secret store.
    pub has_cookies: bool,
    /// Page the download came from, sent as the Referer.
    pub referer: Option<String>,
    /// The browser's User-Agent, sent with every request.
    pub user_agent: Option<String>,
    /// A client holding the cookies, Referer and User-Agent; none means the
    /// daemon's shared client.
    pub client: Option<reqwest::Client>,
}

impl TaskRecord {
    pub fn new(
        id: String,
        url: String,
        directory: PathBuf,
        requested_name: Option<String>,
        connections: u32,
    ) -> Self {
        let filename = requested_name
            .clone()
            .unwrap_or_else(|| placeholder_name(&url));
        let speed_limiter = Arc::new(RateLimiter::new(0));
        let share_limiter = Arc::new(RateLimiter::new(0));
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
            progress: Arc::new(Progress::with_rate_limiters(
                Arc::clone(&share_limiter),
                Arc::clone(&speed_limiter),
            )),
            cancel_tx: None,
            error_message: None,
            speed: SpeedMeter::default(),
            bytes_at_start: 0,
            retries: 0,
            retry_at: None,
            connections,
            speed_limiter,
            share_limiter,
            run_started: Instant::now(),
            moved_at: Instant::now(),
            running: false,
            resume_after_restart: false,
            delete_on_stop: false,
            has_cookies: false,
            referer: None,
            user_agent: None,
            client: None,
        }
    }

    pub fn from_saved(saved: SavedDownload) -> Self {
        let mut rec = Self::new(
            saved.id,
            saved.url,
            saved.directory,
            saved.requested_name,
            saved.connections,
        );
        rec.filename = saved.filename;
        rec.output = saved.output;
        rec.total_bytes = saved.total_bytes;
        rec.downloaded_bytes = saved.downloaded_bytes;
        rec.error_message = saved.error_message;
        rec.speed_limiter.set_bytes_per_sec(saved.max_bytes_per_sec);
        rec.has_cookies = saved.has_cookies;
        rec.referer = saved.referer;
        rec.user_agent = saved.user_agent;
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
            max_bytes_per_sec: self.max_bytes_per_sec(),
            has_cookies: self.has_cookies,
            referer: self.referer.clone(),
            user_agent: self.user_agent.clone(),
        }
    }

    /// Give this download a new context: its client, Referer, User-Agent and
    /// whether it holds cookies. The caller keeps the secret store in step.
    pub fn set_context(&mut self, ctx: &RequestContext, client: Option<reqwest::Client>) {
        self.has_cookies = !ctx.cookies.is_empty();
        self.referer = ctx.referer.clone();
        self.user_agent = ctx.user_agent.clone();
        self.client = client;
    }

    /// Forget the cookies, keeping the Referer and User-Agent. Returns whether
    /// there were any, so the caller knows to delete the secret.
    pub fn drop_cookies(&mut self) -> bool {
        let had = self.has_cookies;
        self.has_cookies = false;
        let ctx = RequestContext {
            cookies: Vec::new(),
            referer: self.referer.clone(),
            user_agent: self.user_agent.clone(),
        };
        self.client = if ctx.is_empty() {
            None
        } else {
            crate::request_context::client_for(&ctx, crate::engine::Timeouts::default()).ok()
        };
        had
    }

    /// This download's own speed cap; 0 means none.
    pub fn max_bytes_per_sec(&self) -> u64 {
        self.speed_limiter.bytes_per_sec()
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
            max_bytes_per_sec: self.max_bytes_per_sec(),
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

    /// True when this download is waiting to retry and its wait is over.
    pub fn retry_is_due(&self, now: Instant) -> bool {
        self.status == DownloadStatus::Retrying
            && !self.running
            && self.retry_at.is_some_and(|at| at <= now)
    }

    /// Record how an engine run ended. The engine reports every stop it was asked
    /// for as a pause, so a stopped download keeps the status it was given when it
    /// was stopped: cancelled, paused, or queued (sent back to the queue to free a slot).
    pub fn apply_outcome(&mut self, outcome: Result<Outcome, Error>) {
        self.sync_from_progress();
        self.downloaded_bytes = self.progress.downloaded.load(Ordering::Relaxed);
        self.speed_bps = 0;
        self.eta_secs = None;
        self.cancel_tx = None;
        self.running = false;
        let cancelled = self.status == DownloadStatus::Cancelled;
        let stopped_by_request = matches!(
            self.status,
            DownloadStatus::Cancelled | DownloadStatus::Queued | DownloadStatus::Paused
        );

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
                self.retries = 0;
                self.retry_at = None;
                self.has_cookies = false;
                self.client = None;
            }
            Ok(Outcome::Paused { reason, state: _ }) => {
                let link_expired = matches!(reason, PauseReason::LinkExpired { .. });
                if link_expired || !stopped_by_request {
                    self.status = DownloadStatus::Paused;
                }
                if let PauseReason::LinkExpired { status } = reason {
                    self.error_message = Some(if self.has_cookies && is_sign_in_status(status) {
                        self.drop_cookies();
                        sign_in_expired_message(status)
                    } else {
                        link_status_message(status)
                    });
                }
            }
            // Stopped during the server check, before any file existed: the
            // stop that was asked for, not a failure. Settles like any stop.
            Err(Error::Cancelled) => {
                if !stopped_by_request {
                    self.status = DownloadStatus::Paused;
                }
            }
            Err(e) if cancelled => self.error_message = Some(e.to_string()),
            // The site turned the cookies down at the first request: they are
            // no use any more, so they go, and the user is told what to do.
            Err(Error::LinkExpired { status }) if self.has_cookies && is_sign_in_status(status) => {
                self.fail_signed_out(sign_in_expired_message(status));
            }
            Err(Error::WebPage) if self.has_cookies => {
                self.fail_signed_out(sign_in_page_message());
            }
            Err(e) => self.fail_or_retry(e),
        }
    }

    /// Stop for good because the site no longer accepts the download's sign-in.
    fn fail_signed_out(&mut self, message: String) {
        self.drop_cookies();
        self.status = DownloadStatus::Failed;
        self.retry_at = None;
        self.error_message = Some(message);
    }

    /// Schedule an automatic retry for network or server trouble, unless this
    /// download has already retried `MAX_AUTO_RETRIES` times without progress.
    fn fail_or_retry(&mut self, e: Error) {
        if self.downloaded_bytes > self.bytes_at_start {
            self.retries = 0;
        }
        if !e.is_transient() || self.retries >= MAX_AUTO_RETRIES {
            self.status = DownloadStatus::Failed;
            self.retry_at = None;
            self.error_message = Some(e.to_string());
            return;
        }
        let delay = auto_retry_delay(self.retries);
        self.retries += 1;
        self.status = DownloadStatus::Retrying;
        self.retry_at = Some(Instant::now() + delay);
        self.error_message = Some(format!(
            "{e}; retry {} of {MAX_AUTO_RETRIES} in {}s",
            self.retries,
            delay.as_secs()
        ));
    }

    /// Get ready for a new engine run.
    pub fn begin_run(&mut self) {
        self.speed.reset();
        self.run_started = Instant::now();
        self.moved_at = self.run_started;
        self.bytes_at_start = self.downloaded_bytes;
        self.retry_at = None;
    }

    /// True once this run has lasted a whole speed window, so its speed means something.
    pub fn speed_is_settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.run_started) >= crate::daemon::speed::SPEED_WINDOW
    }

    /// Update speed and time left from the engine's byte count at `now`.
    /// Returns the byte count.
    pub fn measure(&mut self, now: Instant) -> u64 {
        let bytes = self.progress.downloaded.load(Ordering::Relaxed);
        if bytes != self.downloaded_bytes {
            self.moved_at = now;
        }
        self.downloaded_bytes = bytes;
        self.speed_bps = self.speed.record(now, bytes);
        self.eta_secs = match self.total_bytes {
            Some(total) if bytes >= total => Some(0),
            Some(total) if self.speed_bps > 0 => Some((total - bytes) / self.speed_bps),
            _ => None,
        };
        bytes
    }
}

impl TaskRecord {
    /// Why this active download has had no new data for a while, if it hasn't.
    pub fn stall(&self, now: Instant) -> Option<String> {
        if self.status != DownloadStatus::Active {
            return None;
        }
        let retry = *self.progress.last_retry.lock().unwrap();
        crate::daemon::stall::describe(self.moved_at, retry, now)
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
        );
        rec.status = status;
        rec
    }

    #[test]
    fn an_active_download_without_new_data_says_it_has_stalled() {
        let mut rec = record(DownloadStatus::Active);
        rec.begin_run();
        let started = rec.moved_at;
        rec.measure(started + Duration::from_secs(6));
        assert_eq!(
            rec.stall(started + Duration::from_secs(6)).as_deref(),
            Some("no data for 6s")
        );

        rec.progress.downloaded.store(4096, Ordering::Relaxed);
        rec.measure(started + Duration::from_secs(7));
        assert_eq!(rec.stall(started + Duration::from_secs(8)), None);
    }

    #[test]
    fn only_an_active_download_can_be_stalled() {
        let mut rec = record(DownloadStatus::Paused);
        rec.begin_run();
        assert_eq!(rec.stall(rec.moved_at + Duration::from_secs(60)), None);
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
    fn a_download_sent_back_to_the_queue_stays_queued_when_its_engine_stops() {
        let mut rec = record(DownloadStatus::Queued);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::Cancelled,
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert_eq!(rec.status, DownloadStatus::Queued);
        assert!(!rec.running);
    }

    #[test]
    fn the_speed_cap_is_saved_and_restored() {
        let rec = record(DownloadStatus::Paused);
        rec.speed_limiter.set_bytes_per_sec(48 * 1024);
        let saved = rec.to_saved();
        assert_eq!(saved.max_bytes_per_sec, 48 * 1024);
        let restored = TaskRecord::from_saved(saved);
        assert_eq!(restored.to_summary().max_bytes_per_sec, 48 * 1024);
    }

    fn fail(rec: &mut TaskRecord, e: Error) {
        rec.status = DownloadStatus::Active;
        rec.running = true;
        rec.apply_outcome(Err(e));
    }

    #[test]
    fn a_401_pauses_the_download_and_says_it_needs_sign_in() {
        let mut rec = record(DownloadStatus::Active);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status: 401 },
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert_eq!(rec.status, DownloadStatus::Paused);
        assert_eq!(
            rec.error_message.as_deref(),
            Some("link needs sign-in (HTTP 401)")
        );
    }

    #[test]
    fn a_404_keeps_the_link_expired_message() {
        let mut rec = record(DownloadStatus::Active);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status: 404 },
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert_eq!(
            rec.error_message.as_deref(),
            Some("link expired or removed (HTTP 404)")
        );
    }

    #[test]
    fn a_web_page_fails_the_download_without_a_retry() {
        let mut rec = record(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::WebPage);
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert_eq!(rec.retry_at, None);
        assert_eq!(
            rec.error_message.as_deref(),
            Some("the link opened a web page, not a file")
        );
    }

    #[test]
    fn network_trouble_schedules_a_retry_with_a_growing_wait() {
        let mut rec = record(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::Protocol("status 500".into()));
        assert_eq!(rec.status, DownloadStatus::Retrying);
        assert!(!rec.retry_is_due(Instant::now()));
        assert!(rec.retry_is_due(Instant::now() + auto_retry_delay(0)));
        assert!(rec
            .error_message
            .as_deref()
            .unwrap()
            .contains("retry 1 of 5 in 3s"));

        assert_eq!(auto_retry_delay(1), Duration::from_secs(6));
        assert_eq!(auto_retry_delay(30), MAX_RETRY_DELAY);
    }

    #[test]
    fn retries_without_progress_run_out_and_the_download_fails() {
        let mut rec = record(DownloadStatus::Active);
        for _ in 0..MAX_AUTO_RETRIES {
            rec.begin_run();
            fail(&mut rec, Error::Protocol("status 500".into()));
            assert_eq!(rec.status, DownloadStatus::Retrying);
        }
        rec.begin_run();
        fail(&mut rec, Error::Protocol("status 500".into()));
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert_eq!(rec.retry_at, None);
    }

    #[test]
    fn a_run_that_made_progress_resets_the_retry_count() {
        let mut rec = record(DownloadStatus::Active);
        rec.retries = MAX_AUTO_RETRIES;
        rec.begin_run();
        rec.progress.downloaded.store(4096, Ordering::Relaxed);
        fail(&mut rec, Error::Protocol("connection dropped".into()));
        assert_eq!(rec.status, DownloadStatus::Retrying);
        assert_eq!(rec.retries, 1);
    }

    #[test]
    fn problems_that_will_not_go_away_fail_at_once() {
        let mut rec = record(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::FileChanged);
        assert_eq!(rec.status, DownloadStatus::Failed);
    }

    #[test]
    fn time_left_comes_from_the_averaged_speed() {
        let mut rec = record(DownloadStatus::Active);
        rec.total_bytes = Some(10_000);
        let t0 = Instant::now();
        rec.measure(t0);
        rec.progress.downloaded.store(1000, Ordering::Relaxed);
        rec.measure(t0 + Duration::from_secs(1));
        assert_eq!(rec.speed_bps, 1000);
        assert_eq!(rec.eta_secs, Some(9));
    }

    #[test]
    fn a_stop_during_the_server_check_keeps_the_status_it_was_given() {
        // Stopped before any file exists, the engine reports Cancelled as an error.
        for status in [
            DownloadStatus::Paused,
            DownloadStatus::Queued,
            DownloadStatus::Cancelled,
        ] {
            let mut rec = record(status);
            rec.apply_outcome(Err(Error::Cancelled));
            assert_eq!(rec.status, status);
            assert_eq!(rec.error_message, None, "{status:?}");
            assert_eq!(rec.retry_at, None, "{status:?}");
        }

        // A running download being paused settles as paused, as it does when
        // the engine stops after the check.
        let mut rec = record(DownloadStatus::Pausing);
        rec.apply_outcome(Err(Error::Cancelled));
        assert_eq!(rec.status, DownloadStatus::Paused);
        assert_eq!(rec.error_message, None);
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

    fn with_cookies(status: DownloadStatus) -> TaskRecord {
        let mut rec = record(status);
        rec.has_cookies = true;
        rec.referer = Some("https://example.com/page".to_string());
        rec.user_agent = Some("TestBrowser/1".to_string());
        rec
    }

    #[test]
    fn request_context_survives_a_save_and_restore() {
        let rec = with_cookies(DownloadStatus::Paused);
        let restored = TaskRecord::from_saved(rec.to_saved());
        assert!(restored.has_cookies);
        assert_eq!(restored.referer, rec.referer);
        assert_eq!(restored.user_agent, rec.user_agent);
        assert!(restored.client.is_none(), "the core rebuilds the client");
    }

    #[test]
    fn a_403_on_a_download_with_cookies_says_the_sign_in_expired_and_drops_them() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status: 403 },
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert_eq!(rec.status, DownloadStatus::Paused);
        assert!(!rec.has_cookies);
        assert_eq!(
            rec.error_message.as_deref(),
            Some("sign-in expired (HTTP 403); start the download again from the browser to continue it")
        );
        assert_eq!(rec.referer.as_deref(), Some("https://example.com/page"));
        assert!(rec.client.is_some(), "Referer and User-Agent stay in force");
    }

    #[test]
    fn a_404_on_a_download_with_cookies_keeps_them_and_the_usual_message() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status: 404 },
            state: PathBuf::from("/downloads/ubuntu.iso.part.rdm"),
        }));
        assert!(rec.has_cookies);
        assert_eq!(
            rec.error_message.as_deref(),
            Some("link expired or removed (HTTP 404)")
        );
    }

    #[test]
    fn a_401_at_the_first_request_drops_the_cookies_too() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::LinkExpired { status: 401 });
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert!(!rec.has_cookies);
        assert!(rec
            .error_message
            .as_deref()
            .unwrap()
            .starts_with("sign-in expired (HTTP 401)"));
    }

    #[test]
    fn a_sign_in_page_on_a_download_with_cookies_asks_for_a_new_sign_in() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::WebPage);
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert!(!rec.has_cookies);
        assert_eq!(
            rec.error_message.as_deref(),
            Some("sign-in expired (the site sent a sign-in page); start the download again from the browser to continue it")
        );
    }

    #[test]
    fn network_trouble_keeps_the_cookies() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.begin_run();
        fail(&mut rec, Error::Protocol("status 500".into()));
        assert_eq!(rec.status, DownloadStatus::Retrying);
        assert!(rec.has_cookies);
    }

    #[test]
    fn a_finished_download_lets_go_of_its_cookies_and_client() {
        let mut rec = with_cookies(DownloadStatus::Active);
        rec.running = true;
        rec.apply_outcome(Ok(Outcome::Completed {
            path: PathBuf::from("/downloads/ubuntu.iso"),
            steals: 0,
        }));
        assert!(!rec.has_cookies);
        assert!(rec.client.is_none());
    }
}
