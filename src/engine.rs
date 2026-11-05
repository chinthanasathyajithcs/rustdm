use crate::error::Error;
use crate::probe::{self, Probe};
use crate::ranges::{ByteRange, RangeSet};
use crate::state::{self, part_path, State, STATE_VERSION};
use crate::template::{self, Tokens};
use crate::writer::PartFile;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_CONNECTIONS: u32 = 8;

pub struct Progress {
    pub downloaded: AtomicU64,
    /// Total size in bytes once the probe has run; 0 while unknown.
    pub total: AtomicU64,
    pub completed: Mutex<RangeSet>,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            completed: Mutex::new(RangeSet::new()),
        }
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

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("rustdm/{}", crate::VERSION))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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
    let base_name = template::pick_name(probe.content_disposition.as_deref(), &probe.final_url);
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
    let probe_result = probe::probe(client, &req.url).await?;
    let final_path = resolve_output(&req, &probe_result);
    let part_p = part_path(&final_path);
    let state_p = state::state_path(&final_path);

    if let Some(total) = probe_result.total {
        crate::disk::ensure_disk_space(&part_p, total, crate::disk::SAFETY_MARGIN_BYTES)?;
        req.progress.total.store(total, Ordering::SeqCst);
    }

    let part_file = PartFile::create(&part_p, probe_result.total)?;
    let part_arc = Arc::new(part_file);

    let conns = if req.connections == 0 {
        DEFAULT_CONNECTIONS
    } else {
        req.connections.clamp(1, 16)
    };

    let initial_state = State {
        version: STATE_VERSION,
        url: req.url.to_string(),
        final_url: probe_result.final_url.to_string(),
        total: probe_result.total.unwrap_or(0),
        validator: probe_result.validator.clone(),
        completed: vec![],
        output: final_path.clone(),
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
            &req,
            Some(&state_p),
            Some(&initial_state),
        )
        .await
        .map(|_| 0)
    };

    match result {
        Ok(steals) => {
            let part_file = Arc::try_unwrap(part_arc).map_err(|_| {
                Error::Protocol("failed to acquire ownership of part file".to_string())
            })?;
            part_file.finish(&final_path)?;
            let _ = std::fs::remove_file(&state_p);

            Ok(Outcome::Completed {
                path: final_path,
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

    let probe_result = probe::probe(client, &target_url).await?;

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

    let progress = custom_progress.unwrap_or_else(|| Arc::new(Progress::new()));
    progress.total.store(state.total, Ordering::SeqCst);
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
            &missing_ranges,
            Arc::clone(&part_arc),
            Arc::clone(&progress),
            DEFAULT_CONNECTIONS,
            Some(state_path),
            Some(&state),
            cancel,
        )
        .await
    } else {
        Ok(0)
    };

    match result {
        Ok(steals) => {
            let part_file = Arc::try_unwrap(part_arc).map_err(|_| {
                Error::Protocol("failed to acquire ownership of part file".to_string())
            })?;
            part_file.finish(&state.output)?;
            let _ = std::fs::remove_file(state_path);

            Ok(Outcome::Completed {
                path: state.output,
                steals,
            })
        }
        Err(Error::Cancelled) => Ok(Outcome::Paused {
            reason: PauseReason::Cancelled,
            state: state_path.to_path_buf(),
        }),
        Err(Error::LinkExpired { status }) => Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { status },
            state: state_path.to_path_buf(),
        }),
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_ranges(
    client: &reqwest::Client,
    final_url: &url::Url,
    validator: &probe::Validator,
    segments: &[ByteRange],
    part: Arc<PartFile>,
    progress: Arc<Progress>,
    _conns: u32,
    state_file_path: Option<&Path>,
    state_template: Option<&State>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<u32, Error> {
    let now = Instant::now();
    let lanes: Arc<std::sync::RwLock<Vec<crate::scheduler::Lane>>> =
        Arc::new(std::sync::RwLock::new(
            segments
                .iter()
                .map(|s| crate::scheduler::Lane::new(s.start, s.end, now))
                .collect(),
        ));

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

    for i in 0..segments.len() {
        let client_clone = client.clone();
        let url_clone = final_url.clone();
        let validator_clone = validator.clone();
        let part_clone = Arc::clone(&part);
        let prog_clone = Arc::clone(&progress);
        let lanes_clone = Arc::clone(&lanes);
        let steals_clone = Arc::clone(&steals_count);
        let cancel_clone = cancel.clone();

        set.spawn(async move {
            let initial_lane = {
                let guard = lanes_clone.read().unwrap();
                guard[i].clone()
            };

            crate::worker::fetch_lane(
                &client_clone,
                &url_clone,
                &validator_clone,
                Arc::new(initial_lane),
                Arc::clone(&part_clone),
                Arc::clone(&prog_clone),
                cancel_clone.clone(),
            )
            .await?;

            loop {
                let stolen_range = {
                    let guard = lanes_clone.read().unwrap();
                    if let Some(victim_idx) = crate::scheduler::pick_victim(&guard, Instant::now())
                    {
                        crate::scheduler::steal(&guard[victim_idx])
                    } else {
                        None
                    }
                };

                let Some(range) = stolen_range else {
                    break;
                };

                steals_clone.fetch_add(1, Ordering::SeqCst);
                let stolen_lane =
                    crate::scheduler::Lane::new(range.start, range.end, Instant::now());
                {
                    lanes_clone.write().unwrap().push(stolen_lane.clone());
                }

                crate::worker::fetch_lane(
                    &client_clone,
                    &url_clone,
                    &validator_clone,
                    Arc::new(stolen_lane),
                    Arc::clone(&part_clone),
                    Arc::clone(&prog_clone),
                    cancel_clone.clone(),
                )
                .await?;
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

async fn download_single(
    client: &reqwest::Client,
    probe: &Probe,
    part: Arc<PartFile>,
    req: &DownloadRequest,
    state_file_path: Option<&Path>,
    state_template: Option<&State>,
) -> Result<(), Error> {
    let mut res = client
        .get(probe.final_url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await?;

    if !res.status().is_success() {
        let status = res.status().as_u16();
        if status == 403 || status == 404 || status == 410 {
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
    while let Some(chunk) = res.chunk().await? {
        if let Some(ref rx) = req.cancel {
            if *rx.borrow() {
                if let (Some(sp), Some(st)) = (state_file_path, state_template) {
                    let completed_snapshot: Vec<ByteRange> = {
                        let guard = req.progress.completed.lock().unwrap();
                        guard.ranges().to_vec()
                    };
                    let _ = part.sync();
                    let mut s = st.clone();
                    s.completed = completed_snapshot;
                    let _ = state::save_atomic(sp, &s);
                }
                return Err(Error::Cancelled);
            }
        }

        let chunk_len = chunk.len() as u64;
        let part_clone = Arc::clone(&part);
        let curr_offset = offset;
        tokio::task::spawn_blocking(move || part_clone.write_at(curr_offset, &chunk))
            .await
            .map_err(|e| Error::Protocol(e.to_string()))??;

        offset += chunk_len;
        req.progress
            .downloaded
            .fetch_add(chunk_len, Ordering::SeqCst);
        req.progress.completed.lock().unwrap().insert(ByteRange {
            start: curr_offset,
            end: offset,
        });
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
