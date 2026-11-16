use crate::cancel::{or_cancel, sleep_or_cancel};
use crate::error::{is_link_status, Error};
use crate::limiter::{throttle_all, RateLimiter};
use crate::probe::{self, Probe};
use crate::ranges::{ByteRange, RangeSet};
use crate::retry::{RetryCause, RetryNote};
use crate::scheduler::{pick_victim, steal, Crew, Lane};
use crate::state::{self, part_path, State, STATE_VERSION};
use crate::template::{self, Tokens};
use crate::worker::LaneEnd;
use crate::writer::PartFile;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_CONNECTIONS: u32 = 8;
/// Most connections one download may open.
pub const MAX_CONNECTIONS: u32 = 16;

pub struct Progress {
    pub downloaded: AtomicU64,
    /// Total size in bytes once the probe has run; 0 while unknown.
    pub total: AtomicU64,
    pub completed: Mutex<RangeSet>,
    /// Final file path once the engine has chosen it.
    pub output: Mutex<Option<PathBuf>>,
    /// Optional cap shared with other downloads on their combined speed.
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// Optional cap on this download's own speed.
    pub download_limiter: Option<Arc<RateLimiter>>,
    /// The connection lanes of a multi-connection download, once it has started.
    pub lanes: Mutex<Option<SharedLanes>>,
    /// The latest reason a connection had to try again, if any.
    pub last_retry: Mutex<Option<RetryNote>>,
}

type SharedLanes = Arc<std::sync::RwLock<Vec<crate::scheduler::Lane>>>;

/// One connection's share of the file at a moment in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub start: u64,
    /// Next byte this connection will write.
    pub cursor: u64,
    pub end: u64,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            completed: Mutex::new(RangeSet::new()),
            output: Mutex::new(None),
            rate_limiter: None,
            download_limiter: None,
            lanes: Mutex::new(None),
            last_retry: Mutex::new(None),
        }
    }

    pub fn with_rate_limiter(limiter: Arc<RateLimiter>) -> Self {
        Self {
            rate_limiter: Some(limiter),
            ..Self::new()
        }
    }

    /// Progress for a download held to both a shared cap and its own cap.
    pub fn with_rate_limiters(shared: Arc<RateLimiter>, own: Arc<RateLimiter>) -> Self {
        Self {
            rate_limiter: Some(shared),
            download_limiter: Some(own),
            ..Self::new()
        }
    }

    /// Wait until `bytes` may be written under this download's speed caps.
    pub async fn throttle(
        &self,
        bytes: u64,
        cancel: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> Result<(), Error> {
        let limiters = [&self.rate_limiter, &self.download_limiter];
        throttle_all(limiters.into_iter().flatten().map(|l| &**l), bytes, cancel).await
    }

    /// Record that a connection hit `cause` and will try again after `wait`.
    pub fn note_retry(&self, cause: RetryCause, wait: Duration) {
        let at = Instant::now();
        *self.last_retry.lock().unwrap() = Some(RetryNote {
            cause,
            at,
            next_at: at + wait,
        });
    }

    /// Current position of every connection lane; empty for single-connection downloads.
    pub fn lane_snapshots(&self) -> Vec<LaneSnapshot> {
        let Some(lanes) = self.lanes.lock().unwrap().clone() else {
            return Vec::new();
        };
        let lanes = lanes.read().unwrap();
        lanes
            .iter()
            .map(|lane| {
                let cursor = lane.cursor.load(Ordering::SeqCst);
                let done = lane.bytes_done.load(Ordering::SeqCst);
                LaneSnapshot {
                    start: cursor.saturating_sub(done),
                    cursor,
                    end: lane.end.load(Ordering::SeqCst),
                }
            })
            .collect()
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DownloadRequest {
    pub url: url::Url,
    pub template: String,
    /// File name to use for `{name}` instead of the server's; sanitized like any name.
    pub file_name: Option<String>,
    pub downloads_dir: PathBuf,
    pub connections: u32,
    pub progress: Arc<Progress>,
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseReason {
    Cancelled,
    LinkExpired { status: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed { path: PathBuf, steals: u32 },
    Paused { reason: PauseReason, state: PathBuf },
}

/// How long the HTTP client waits on a server before treating the attempt as failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Time allowed to open a connection.
    pub connect: Duration,
    /// Longest silence allowed while waiting for data; any data resets it.
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            read: Duration::from_secs(30),
        }
    }
}

pub fn build_client() -> reqwest::Client {
    build_client_with(Timeouts::default())
}

pub fn build_client_with(timeouts: Timeouts) -> reqwest::Client {
    client_builder(timeouts)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The settings every download client shares: the default User-Agent and the
/// timeouts. A client with a request context starts from the same builder.
pub fn client_builder(timeouts: Timeouts) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(format!("rustdm/{}", crate::VERSION))
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.read)
}

/// An I/O error that names the file or folder it is about.
fn path_error(action: &str, path: &Path, err: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(
        err.kind(),
        format!("cannot {action} {}: {err}", path.display()),
    ))
}

fn utc_date_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142400) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn resolve_output(req: &DownloadRequest, probe: &Probe) -> PathBuf {
    let base_name = match &req.file_name {
        Some(name) => name.clone(),
        None => template::pick_name(probe.content_disposition.as_deref(), &probe.final_url),
    };
    let host = probe.final_url.host_str().unwrap_or("").to_string();
    let date = utc_date_str();

    let mut current_name = base_name.clone();
    let mut collision_counter = 0;

    loop {
        let tokens = Tokens {
            downloads: req.downloads_dir.to_string_lossy().to_string(),
            host: host.clone(),
            name: current_name.clone(),
            date: date.clone(),
        };

        let expanded = template::expand(&req.template, &tokens);
        let candidate = PathBuf::from(expanded);
        let part = part_path(&candidate);

        if !candidate.exists() && !part.exists() {
            return candidate;
        }

        collision_counter += 1;
        current_name = template::collision_name(&base_name, collision_counter);
    }
}

pub async fn download(client: &reqwest::Client, req: DownloadRequest) -> Result<Outcome, Error> {
    let probe_result =
        probe::probe_for(client, &req.url, req.cancel.as_ref(), Some(&req.progress)).await?;
    if probe_result.is_unwanted_web_page() {
        return Err(Error::WebPage);
    }
    let final_path = resolve_output(&req, &probe_result);
    *req.progress.output.lock().unwrap() = Some(final_path.clone());
    let part_p = part_path(&final_path);
    let state_p = state::state_path(&final_path);

    if let Some(folder) = final_path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| path_error("create folder", folder, e))?;
    }
    if let Some(total) = probe_result.total {
        crate::disk::ensure_disk_space(&part_p, total, crate::disk::SAFETY_MARGIN_BYTES)?;
        req.progress.total.store(total, Ordering::SeqCst);
    }

    let part_file = PartFile::create(&part_p, probe_result.total)
        .map_err(|e| path_error("create", &part_p, e))?;
    let part_arc = Arc::new(part_file);

    let conns = if req.connections == 0 {
        DEFAULT_CONNECTIONS
    } else {
        req.connections.clamp(1, MAX_CONNECTIONS)
    };

    let initial_state = State {
        version: STATE_VERSION,
        url: req.url.to_string(),
        final_url: probe_result.final_url.to_string(),
        total: probe_result.total.unwrap_or(0),
        validator: probe_result.validator.clone(),
        completed: vec![],
        output: final_path.clone(),
        connections: Some(conns),
    };
    state::save_atomic(&state_p, &initial_state)?;

    let result = if probe_result.ranges_supported && probe_result.total.is_some_and(|t| t > 0) {
        let total = probe_result.total.unwrap();
        let segments = crate::ranges::initial_segments(total, conns);
        download_ranges(
            client,
            &probe_result.final_url,
            &probe_result.validator,
            &segments,
            Arc::clone(&part_arc),
            Arc::clone(&req.progress),
            conns,
            Some(&state_p),
            Some(&initial_state),
            req.cancel.clone(),
        )
        .await
    } else {
        download_single(
            client,
            &probe_result,
            Arc::clone(&part_arc),
            &req.progress,
            req.cancel.as_ref(),
            Some(&state_p),
            Some(&initial_state),
        )
        .await
        .map(|_| 0)
    };

    conclude(result, part_arc, final_path, state_p)
}

/// Turn the end of an engine run into an outcome: on success, move the part
/// file into place and drop its state file; on a stop, report the pause.
fn conclude(
    result: Result<u32, Error>,
    part: Arc<PartFile>,
    output: PathBuf,
    state_p: PathBuf,
) -> Result<Outcome, Error> {
    match result {
        Ok(steals) => {
            let part_file = Arc::try_unwrap(part).map_err(|_| {
                Error::Protocol("failed to acquire ownership of part file".to_string())
            })?;
            part_file.finish(&output)?;
            let _ = std::fs::remove_file(&state_p);
            Ok(Outcome::Completed {
                path: output,
                steals,
            })
        }
        Err(Error::Cancelled) => Ok(Outcome::Paused {
            reason: PauseReason::Cancelled,
            state: state_p,
        }),
        Err(Error::LinkExpired { status }) => Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status },
            state: state_p,
        }),
        Err(err) => Err(err),
    }
}

fn validator_matches(expected: &probe::Validator, actual: &probe::Validator) -> bool {
    if let (Some(e1), Some(e2)) = (&expected.etag, &actual.etag) {
        return e1 == e2;
    }
    if let (Some(lm1), Some(lm2)) = (&expected.last_modified, &actual.last_modified) {
        return lm1 == lm2;
    }
    expected == actual
}

pub async fn resume(
    client: &reqwest::Client,
    state_path: &Path,
    url_override: Option<url::Url>,
) -> Result<Outcome, Error> {
    resume_with_options(client, state_path, url_override, None, None).await
}

pub async fn resume_with_options(
    client: &reqwest::Client,
    state_path: &Path,
    url_override: Option<url::Url>,
    custom_progress: Option<Arc<Progress>>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Outcome, Error> {
    let state = state::load(state_path)?;

    let part_p = state::part_path(&state.output);
    if !part_p.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "part file not found",
        )));
    }

    let target_url = if let Some(u) = url_override {
        u
    } else {
        url::Url::parse(&state.url).map_err(|e| Error::Protocol(e.to_string()))?
    };

    let progress = custom_progress.unwrap_or_else(|| Arc::new(Progress::new()));
    let probe_result =
        probe::probe_for(client, &target_url, cancel.as_ref(), Some(&progress)).await?;
    // A link that now opens a sign-in screen would be saved over the real file.
    if probe_result.is_unwanted_web_page() {
        return Err(Error::WebPage);
    }

    // Without range support or a known size, the only way on is from byte 0.
    if !probe_result.ranges_supported || state.total == 0 {
        tracing::info!(
            "{} cannot be resumed from where it stopped; starting it over",
            state.output.display()
        );
        return restart(client, &state, state_path, &probe_result, progress, cancel).await;
    }

    if probe_result.total != Some(state.total)
        || !validator_matches(&state.validator, &probe_result.validator)
    {
        return Err(Error::FileChanged);
    }

    let part_file = PartFile::open(&part_p)?;
    let part_arc = Arc::new(part_file);

    let mut range_set = RangeSet::new();
    for r in &state.completed {
        range_set.insert(*r);
    }
    let missing_ranges = range_set.complement(state.total);
    let conns = state
        .connections
        .unwrap_or(DEFAULT_CONNECTIONS)
        .clamp(1, MAX_CONNECTIONS);

    progress.total.store(state.total, Ordering::SeqCst);
    *progress.output.lock().unwrap() = Some(state.output.clone());
    progress
        .downloaded
        .store(range_set.covered(), Ordering::SeqCst);
    {
        *progress.completed.lock().unwrap() = range_set;
    }

    let result = if !missing_ranges.is_empty() {
        download_ranges(
            client,
            &probe_result.final_url,
            &probe_result.validator,
            &crate::ranges::resume_lanes(&missing_ranges, conns),
            Arc::clone(&part_arc),
            Arc::clone(&progress),
            conns,
            Some(state_path),
            Some(&state),
            cancel,
        )
        .await
    } else {
        Ok(0)
    };

    conclude(result, part_arc, state.output, state_path.to_path_buf())
}

/// Download the whole file again into the paused download's place, for servers
/// that cannot send part of it.
async fn restart(
    client: &reqwest::Client,
    old: &State,
    state_path: &Path,
    probe_result: &Probe,
    progress: Arc<Progress>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Outcome, Error> {
    let part_p = state::part_path(&old.output);
    std::fs::remove_file(&part_p).map_err(|e| path_error("remove", &part_p, e))?;
    let part = Arc::new(
        PartFile::create(&part_p, probe_result.total)
            .map_err(|e| path_error("create", &part_p, e))?,
    );

    let state = State {
        url: old.url.clone(),
        final_url: probe_result.final_url.to_string(),
        total: probe_result.total.unwrap_or(0),
        validator: probe_result.validator.clone(),
        completed: vec![],
        ..old.clone()
    };
    state::save_atomic(state_path, &state)?;

    progress
        .total
        .store(probe_result.total.unwrap_or(0), Ordering::SeqCst);
    progress.downloaded.store(0, Ordering::SeqCst);
    *progress.completed.lock().unwrap() = RangeSet::new();
    *progress.output.lock().unwrap() = Some(state.output.clone());

    let result = download_single(
        client,
        probe_result,
        Arc::clone(&part),
        &progress,
        cancel.as_ref(),
        Some(state_path),
        Some(&state),
    )
    .await
    .map(|_| 0);
    conclude(result, part, state.output, state_path.to_path_buf())
}

/// How often an idle connection looks again for work near the end of a download,
/// when no lane has ended meanwhile.
const IDLE_POLL: Duration = Duration::from_millis(100);

type Pending = Arc<Mutex<std::collections::VecDeque<ByteRange>>>;

/// What an idle connection does next.
enum Work {
    /// A lane nobody had started, already registered.
    Range(Arc<Lane>),
    /// Half of what another connection had left, already registered.
    Stolen(Arc<Lane>),
    /// Nothing to take yet, but other connections are still at work.
    Wait,
    /// Every byte is written or being written by someone else for good: leave.
    AllDone,
}

/// The next job for an idle connection. Both locks are held while it picks work,
/// registers the lane and decides to leave, so no range in hand is ever unseen:
/// neither one just taken from the queue nor one handed back ([`hand_back`]).
fn next_work(pending: &Pending, lanes: &SharedLanes, crew: &Crew) -> Work {
    let mut queue = pending.lock().unwrap();
    let mut lanes = lanes.write().unwrap();
    let now = Instant::now();
    let register = |lanes: &mut Vec<Lane>, range: ByteRange| {
        let lane = Lane::new(range.start, range.end, now);
        lanes.push(lane.clone());
        Arc::new(lane)
    };
    if let Some(range) = queue.pop_front() {
        return Work::Range(register(&mut lanes, range));
    }
    if let Some(range) = pick_victim(&lanes, now).and_then(|i| steal(&lanes[i], now)) {
        return Work::Stolen(register(&mut lanes, range));
    }
    if lanes.iter().all(Lane::is_done) {
        crew.leave();
        return Work::AllDone;
    }
    Work::Wait
}

/// A refused connection puts the rest of its lane back for the others and leaves,
/// unless it is the last one, which stays and keeps trying. True when it left.
fn hand_back(lane: &Lane, pending: &Pending, crew: &Crew) -> bool {
    let mut queue = pending.lock().unwrap();
    if let Some(rest) = lane.take_rest() {
        queue.push_back(rest);
    }
    crew.try_leave()
}

#[allow(clippy::too_many_arguments)]
async fn download_ranges(
    client: &reqwest::Client,
    final_url: &url::Url,
    validator: &probe::Validator,
    segments: &[ByteRange],
    part: Arc<PartFile>,
    progress: Arc<Progress>,
    conns: u32,
    state_file_path: Option<&Path>,
    state_template: Option<&State>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<u32, Error> {
    // Lanes are added as workers start them, so stealing only splits work in progress.
    let lanes: SharedLanes = Arc::new(std::sync::RwLock::new(Vec::with_capacity(segments.len())));
    let pending: Pending = Arc::new(Mutex::new(segments.iter().copied().collect()));
    let workers = segments.len().min(conns.max(1) as usize);
    *progress.lanes.lock().unwrap() = Some(Arc::clone(&lanes));

    let steals_count = Arc::new(AtomicU64::new(0));
    let mut set = tokio::task::JoinSet::new();

    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let checkpoint_handle = if let (Some(sp), Some(st)) = (state_file_path, state_template) {
        let sp = sp.to_path_buf();
        let st = st.clone();
        let part_clone = Arc::clone(&part);
        let prog_clone = Arc::clone(&progress);

        Some(tokio::spawn(async move {
            let mut last_bytes = prog_clone.downloaded.load(Ordering::SeqCst);
            let mut last_time = Instant::now();

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        let cur_bytes = prog_clone.downloaded.load(Ordering::SeqCst);
                        let bytes_diff = cur_bytes.saturating_sub(last_bytes);
                        let time_diff = last_time.elapsed();

                        if state::should_checkpoint(bytes_diff, time_diff) {
                            let completed_snapshot: Vec<ByteRange> = {
                                let guard = prog_clone.completed.lock().unwrap();
                                guard.ranges().to_vec()
                            };
                            let _ = part_clone.sync();
                            let mut s = st.clone();
                            s.completed = completed_snapshot;
                            let _ = state::save_atomic(&sp, &s);

                            last_bytes = cur_bytes;
                            last_time = Instant::now();
                        }
                    }
                    _ = done_rx.changed() => {
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };

    let crew = Arc::new(Crew::new(workers));
    // Wakes idle connections when a lane ends, so the last byte ends the download at once.
    let lane_ended = Arc::new(tokio::sync::Notify::new());
    for _ in 0..workers {
        let client_clone = client.clone();
        let url_clone = final_url.clone();
        let validator_clone = validator.clone();
        let part_clone = Arc::clone(&part);
        let prog_clone = Arc::clone(&progress);
        let lanes_clone = Arc::clone(&lanes);
        let steals_clone = Arc::clone(&steals_count);
        let cancel_clone = cancel.clone();
        let pending_clone = Arc::clone(&pending);
        let crew = Arc::clone(&crew);
        let lane_ended = Arc::clone(&lane_ended);

        set.spawn(async move {
            loop {
                let lane = match next_work(&pending_clone, &lanes_clone, &crew) {
                    Work::Range(lane) => lane,
                    Work::Stolen(lane) => {
                        steals_clone.fetch_add(1, Ordering::SeqCst);
                        lane
                    }
                    // Another connection may still turn out slow, or hand its rest back.
                    Work::Wait => {
                        tokio::select! {
                            _ = lane_ended.notified() => {}
                            waited = sleep_or_cancel(IDLE_POLL, cancel_clone.as_ref()) => waited?,
                        }
                        continue;
                    }
                    Work::AllDone => break,
                };
                let end = crate::worker::fetch_lane(
                    &client_clone,
                    &url_clone,
                    &validator_clone,
                    Arc::clone(&lane),
                    Arc::clone(&part_clone),
                    Arc::clone(&prog_clone),
                    cancel_clone.clone(),
                    Some(&crew),
                )
                .await?;
                let left = end == LaneEnd::Refused && hand_back(&lane, &pending_clone, &crew);
                lane_ended.notify_waiters();
                if left {
                    break;
                }
            }
            Ok::<(), Error>(())
        });
    }

    let mut first_error = None;
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                set.abort_all();
            }
            Err(join_err) => {
                if first_error.is_none() {
                    first_error = Some(Error::Protocol(join_err.to_string()));
                }
                set.abort_all();
            }
        }
    }

    let _ = done_tx.send(true);
    if let Some(h) = checkpoint_handle {
        let _ = h.await;
    }

    let unfinished =
        !pending.lock().unwrap().is_empty() || lanes.read().unwrap().iter().any(|l| !l.is_done());
    if first_error.is_none() && unfinished {
        first_error = Some(Error::Protocol(
            "the connections stopped with parts of the file still missing".to_string(),
        ));
    }

    // If an error occurred (like Cancelled or LinkExpired), save state now
    if let Some(err) = first_error {
        if let (Some(sp), Some(st)) = (state_file_path, state_template) {
            let completed_snapshot: Vec<ByteRange> = {
                let guard = progress.completed.lock().unwrap();
                guard.ranges().to_vec()
            };
            let _ = part.sync();
            let mut s = st.clone();
            s.completed = completed_snapshot;
            let _ = state::save_atomic(sp, &s);
        }
        return Err(err);
    }

    Ok(steals_count.load(Ordering::SeqCst) as u32)
}

#[allow(clippy::too_many_arguments)]
async fn download_single(
    client: &reqwest::Client,
    probe: &Probe,
    part: Arc<PartFile>,
    progress: &Arc<Progress>,
    cancel: Option<&tokio::sync::watch::Receiver<bool>>,
    state_file_path: Option<&Path>,
    state_template: Option<&State>,
) -> Result<(), Error> {
    let request = client
        .get(probe.final_url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send();
    let mut res = or_cancel(request, cancel).await??;

    if !res.status().is_success() {
        let status = res.status().as_u16();
        if is_link_status(status) {
            if let (Some(sp), Some(st)) = (state_file_path, state_template) {
                let _ = part.sync();
                let _ = state::save_atomic(sp, st);
            }
            return Err(Error::LinkExpired { status });
        }
        return Err(Error::Protocol(format!(
            "download failed with status {}",
            res.status()
        )));
    }

    let mut offset = 0u64;
    loop {
        // A pause or throttle cancel saves what is on disk and reports the stop.
        let stopped = match or_cancel(res.chunk(), cancel).await {
            Err(Error::Cancelled) => true,
            Err(e) => return Err(e),
            Ok(next) => match next? {
                None => break,
                Some(chunk) => {
                    let chunk_len = chunk.len() as u64;
                    if progress.throttle(chunk_len, cancel).await.is_err() {
                        true
                    } else {
                        let part_clone = Arc::clone(&part);
                        let curr_offset = offset;
                        tokio::task::spawn_blocking(move || {
                            part_clone.write_at(curr_offset, &chunk)
                        })
                        .await
                        .map_err(|e| Error::Protocol(e.to_string()))??;

                        offset += chunk_len;
                        progress.downloaded.fetch_add(chunk_len, Ordering::SeqCst);
                        progress.completed.lock().unwrap().insert(ByteRange {
                            start: curr_offset,
                            end: offset,
                        });
                        false
                    }
                }
            },
        };
        if stopped {
            if let (Some(sp), Some(st)) = (state_file_path, state_template) {
                let mut s = st.clone();
                s.completed = progress.completed.lock().unwrap().ranges().to_vec();
                let _ = part.sync();
                let _ = state::save_atomic(sp, &s);
            }
            return Err(Error::Cancelled);
        }
    }

    if let Some(total) = probe.total {
        if offset != total {
            return Err(Error::Protocol(format!(
                "expected {total} bytes, but received {offset}"
            )));
        }
    }

    Ok(())
}
