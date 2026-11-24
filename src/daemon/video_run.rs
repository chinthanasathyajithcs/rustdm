//! Running a chosen video: each stream goes through the engine in turn, in
//! requests no larger than the site allows, then the streams become the final
//! file (spec sections 5.3 and 6).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_fields::{
    joining_text, step_text, stream_file_name, video_space_needed, video_total,
};
use super::video_refresh::{link_expired_while_running, video_changed, Refreshed};
use crate::disk::{ensure_disk_space, volume_label, SAFETY_MARGIN_BYTES};
use crate::engine::{self, DownloadRequest, Outcome, Progress, ResumeOptions, Timeouts};
use crate::error::Error;
use crate::ipc::protocol::DownloadStatus;
use crate::ranges::RangeSet;
use crate::request_context::{stream_client, RequestContext};
use crate::state::{part_path, state_path};
use crate::template::collision_name;
use crate::tools::{FetchCtx, Freshness, Installed, Tool, ToolError};
use crate::tui::format::format_bytes;
use crate::video::errors::{join_failed_message, NO_VIDEO};
use crate::video::join::{join, joining_path, JoinFailure, JoinRequest, UNKNOWN_PERCENT};
use crate::video::refresh::REFRESH_STEP;
use crate::video::{Container, StreamKind, StreamPlan, VideoPlan, VideoStep};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use url::Url;

/// Where the engine saves a stream: the name given, in the download's folder.
const STREAM_TEMPLATE: &str = "{downloads}/{name}";
/// Why the disk check asks for twice the size.
const JOINED_NOTE: &str =
    " (the video is joined from two parts, so it briefly needs about twice its size)";

/// What the run does next, read from the record.
enum Next {
    Stream(usize),
    Join,
    /// Joined already: the final file is there.
    Done(PathBuf),
    /// The user stopped the download.
    Stop,
}

/// What a stream's engine run needs, copied out so no lock is held while it runs.
struct StreamJob {
    id: String,
    stream: StreamPlan,
    file_name: String,
    directory: PathBuf,
    connections: u32,
    referer: Option<String>,
    user_agent: Option<String>,
    progress: Arc<Progress>,
}

/// ffmpeg being fetched while the streams download, so the join need not wait.
struct FfmpegFetch {
    handle: JoinHandle<Result<Installed, ToolError>>,
    progress: Arc<Progress>,
}

impl Drop for FfmpegFetch {
    /// A run that ends early does not leave the fetch running on its own.
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A final name held by a running download until its file is placed.
struct NameClaim {
    path: PathBuf,
    claimed: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Drop for NameClaim {
    fn drop(&mut self) {
        self.claimed.lock().unwrap().remove(&self.path);
    }
}

/// What the join needs, copied out of the record.
struct JoinJob {
    video: PathBuf,
    audio: PathBuf,
    container: Container,
    duration_secs: Option<u64>,
    folder: PathBuf,
    /// The free final name; ffmpeg writes `<final_path>.joining`.
    final_path: PathBuf,
    percent: Arc<AtomicU8>,
}

impl DaemonCore {
    /// Run a video whose quality is chosen, from `plan.step` on, until the
    /// download settles. Called with the download `Active` and running.
    pub(super) async fn run_video(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: watch::Receiver<bool>,
    ) {
        if let Err(e) = begin_video_run(record) {
            return self.finish_task(record, Err(e));
        }
        let mut ffmpeg: Option<FfmpegFetch> = None;
        loop {
            match next_step(record, &cancel) {
                Next::Stop => return self.finish_task(record, Err(Error::Cancelled)),
                Next::Stream(index) => {
                    if index == 0 && ffmpeg.is_none() && has_two_streams(record) {
                        ffmpeg = self.start_ffmpeg_fetch(record, &cancel).ok();
                    }
                    let outcome = self.run_stream(record, index, cancel.clone()).await;
                    if link_expired_while_running(&outcome, record, &cancel) {
                        match self.refresh_links(record, &cancel).await {
                            Refreshed::Again => continue,
                            Refreshed::Pause(message) => return self.pause_with(record, message),
                            Refreshed::Settle(e) => return self.finish_task(record, Err(e)),
                        }
                    }
                    match outcome {
                        Ok(Outcome::Completed { path, .. }) => {
                            self.stream_done(record, index, path)
                        }
                        // The stream on the site is no longer the one on disk.
                        Err(Error::FileChanged) => {
                            return self.finish_task(record, Err(video_changed()))
                        }
                        other => return self.finish_task(record, other),
                    }
                }
                Next::Join => return self.join_streams(record, ffmpeg, cancel).await,
                Next::Done(path) => {
                    let outcome = complete_at(&path, &stream_files(record));
                    return self.finish_task(record, outcome);
                }
            }
        }
    }

    /// Fetch stream `index`: resumed from its state file when it has one,
    /// else started under its stream file name.
    async fn run_stream(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        index: usize,
        cancel: watch::Receiver<bool>,
    ) -> Result<Outcome, Error> {
        let job = stream_job(record, index)?;
        let ctx = RequestContext {
            cookies: self.cookies_for(&job.id),
            referer: job.referer,
            user_agent: job.user_agent,
        };
        let ctx = Some(&ctx).filter(|ctx| !ctx.is_empty());
        let client = stream_client(ctx, &job.stream.headers, Timeouts::default())?;
        let url = Url::parse(&job.stream.url)
            .map_err(|e| Error::Protocol(format!("invalid stream URL: {e}")))?;
        let cap = job.stream.request_bytes;
        match job.stream.file.as_deref() {
            Some(file) if state_path(file).exists() => {
                let opts = ResumeOptions {
                    url_override: Some(url),
                    progress: Some(job.progress),
                    cancel: Some(cancel),
                    max_request_bytes: cap,
                };
                engine::resume_with(&client, &state_path(file), opts).await
            }
            // Finished before the plan could say so.
            Some(file) if file.is_file() && !part_path(file).exists() => Ok(Outcome::Completed {
                path: file.to_path_buf(),
                steals: 0,
            }),
            _ => {
                let req = DownloadRequest {
                    url,
                    template: STREAM_TEMPLATE.to_string(),
                    file_name: Some(job.file_name),
                    downloads_dir: job.directory,
                    connections: job.connections,
                    progress: job.progress,
                    cancel: Some(cancel),
                    max_request_bytes: cap,
                };
                engine::download(&client, req).await
            }
        }
    }

    /// Stream `index` is complete at `path`: count it and move to the next step.
    fn stream_done(&self, record: &Arc<Mutex<TaskRecord>>, index: usize, path: PathBuf) {
        {
            let mut guard = record.lock().unwrap();
            let rec = &mut *guard;
            let Some(plan) = rec.video.as_mut() else {
                return;
            };
            let Some(stream) = plan.streams.get_mut(index) else {
                return;
            };
            stream.file = Some(path);
            rec.progress_base += stream_bytes(stream);
            plan.step = if index + 1 < plan.streams.len() {
                VideoStep::Stream(index + 1)
            } else {
                VideoStep::Join
            };
            rec.progress = fresh_progress(rec);
        }
        self.persist();
    }

    /// Make the final file of the downloaded streams. One stream is renamed to
    /// the final name; two are joined with ffmpeg.
    async fn join_streams(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        ffmpeg: Option<FfmpegFetch>,
        cancel: watch::Receiver<bool>,
    ) {
        let files = stream_files(record);
        let outcome = match files.as_slice() {
            [_] => {
                // The free name and `Done` are saved before the rename, so a
                // restart in between finds the rename to finish, or finished.
                let claim = self.claim_final_name(&wanted_final(record));
                mark_done(record, &claim.path);
                self.persist();
                complete_at(&claim.path, &files)
            }
            [_, _] => self.join_two_streams(record, ffmpeg, cancel).await,
            _ => Err(no_video()),
        };
        record.lock().unwrap().join_percent = None;
        self.finish_task(record, outcome);
    }

    /// Join the video and the audio into `<final>.joining`, then make that the
    /// final file. The free name is kept in the record before ffmpeg starts, so
    /// a restart joins under the same name; `Done` is saved before the rename,
    /// so a restart in between finds the joined file to rename.
    async fn join_two_streams(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        ffmpeg: Option<FfmpegFetch>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Outcome, Error> {
        // Claimed until the file is placed: another download with the same
        // name would otherwise write over this one's temporary file.
        let mut claim = self.claim_final_name(&wanted_final(record));
        let job = join_job(record, &claim.path)?;
        self.persist();
        let ffmpeg = self.ffmpeg_for(record, ffmpeg, &cancel).await?;
        let temp = joining_path(&job.final_path);
        let request = JoinRequest {
            video: &job.video,
            audio: &job.audio,
            container: job.container,
            temp: &temp,
            duration_secs: job.duration_secs,
            percent: &job.percent,
            cancel,
        };
        join(&ffmpeg, request)
            .await
            .map_err(|failure| join_error(failure, &job.folder))?;
        // The name may have been taken while ffmpeg ran: nothing is overwritten.
        if free_final_path(&claim.path) != claim.path {
            drop(claim);
            claim = self.claim_final_name(&job.final_path);
            std::fs::rename(&temp, joining_path(&claim.path))?;
        }
        mark_done(record, &claim.path);
        self.persist();
        complete_at(&claim.path, &stream_files(record))
    }

    /// Claim the first name from `wanted` on that is not a file and not
    /// claimed by another running download.
    fn claim_final_name(&self, wanted: &Path) -> NameClaim {
        let mut claimed = self.claimed_names.lock().unwrap();
        let path = free_final_path_avoiding(wanted, &claimed);
        claimed.insert(path.clone());
        NameClaim {
            path,
            claimed: Arc::clone(&self.claimed_names),
        }
    }

    /// Start getting ffmpeg in the background, with its progress ready to show.
    fn start_ffmpeg_fetch(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<FfmpegFetch, Error> {
        let Some(tools) = self.tools.clone() else {
            return Err(Error::Tool {
                message: "This daemon has no tools folder.".to_string(),
                transient: false,
            });
        };
        let progress = {
            let rec = record.lock().unwrap();
            Arc::new(Progress::with_rate_limiters(
                Arc::clone(&rec.share_limiter),
                Arc::clone(&rec.speed_limiter),
            ))
        };
        let ctx = FetchCtx {
            progress: Arc::clone(&progress),
            cancel: Some(cancel.clone()),
        };
        let handle = tokio::spawn(async move {
            tools
                .ensure(Tool::Ffmpeg, Freshness::CheckIfStale, ctx)
                .await
        });
        Ok(FfmpegFetch { handle, progress })
    }

    /// ffmpeg, from the fetch begun with the first stream, or begun now (after
    /// a restart). The row shows its progress while it is being fetched.
    async fn ffmpeg_for(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        started: Option<FfmpegFetch>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<Installed, Error> {
        let mut fetch = match started {
            Some(fetch) => fetch,
            None => self.start_ffmpeg_fetch(record, cancel)?,
        };
        record.lock().unwrap().lookup_progress = Some((Tool::Ffmpeg, Arc::clone(&fetch.progress)));
        let result = (&mut fetch.handle).await;
        record.lock().unwrap().lookup_progress = None;
        match result {
            Ok(installed) => Ok(installed?),
            Err(e) => Err(Error::Tool {
                message: format!("Getting ffmpeg was interrupted: {e}"),
                transient: true,
            }),
        }
    }
}

fn has_two_streams(record: &Arc<Mutex<TaskRecord>>) -> bool {
    let rec = record.lock().unwrap();
    rec.video
        .as_ref()
        .is_some_and(|plan| plan.streams.len() > 1)
}

/// Copy out what the join needs, and keep the free final name in the record
/// (the row shows it, and a restart joins to the same temporary file).
fn join_job(record: &Arc<Mutex<TaskRecord>>, final_path: &Path) -> Result<JoinJob, Error> {
    let mut guard = record.lock().unwrap();
    let rec = &mut *guard;
    let plan = rec.video.as_ref().ok_or_else(no_video)?;
    let file_of = |kind| {
        let stream = plan.streams.iter().find(|s| s.kind == kind);
        stream.and_then(|s| s.file.clone())
    };
    let (Some(video), Some(audio)) = (file_of(StreamKind::Video), file_of(StreamKind::Audio))
    else {
        return Err(no_video());
    };
    let container = plan.choice.as_ref().map_or(Container::Mp4, |c| c.container);
    let duration_secs = plan.duration_secs;
    if let Some(name) = final_path.file_name() {
        rec.filename = name.to_string_lossy().into_owned();
    }
    rec.output = Some(final_path.to_path_buf());
    let percent = Arc::new(AtomicU8::new(UNKNOWN_PERCENT));
    rec.join_percent = Some(Arc::clone(&percent));
    rec.step = Some(joining_text(None));
    Ok(JoinJob {
        video,
        audio,
        container,
        duration_secs,
        folder: rec.directory.clone(),
        final_path: final_path.to_path_buf(),
        percent,
    })
}

fn join_error(failure: JoinFailure, folder: &Path) -> Error {
    match failure {
        JoinFailure::Cancelled => Error::Cancelled,
        JoinFailure::Exited { last_line } | JoinFailure::Spawn(last_line) => Error::Video {
            message: join_failed_message(&last_line, folder),
            transient: false,
        },
    }
}

/// Every stream's file, or none when one of them has none yet.
fn stream_files(record: &Arc<Mutex<TaskRecord>>) -> Vec<PathBuf> {
    let rec = record.lock().unwrap();
    let files: Option<Vec<PathBuf>> = rec
        .video
        .iter()
        .flat_map(|plan| &plan.streams)
        .map(|s| s.file.clone())
        .collect();
    files.unwrap_or_default()
}

/// The final name the record asks for, taken or not.
fn wanted_final(record: &Arc<Mutex<TaskRecord>>) -> PathBuf {
    let rec = record.lock().unwrap();
    rec.directory.join(&rec.filename)
}

/// Record the chosen final path, with the step `Done`.
fn mark_done(record: &Arc<Mutex<TaskRecord>>, final_path: &Path) {
    let mut rec = record.lock().unwrap();
    if let Some(name) = final_path.file_name() {
        rec.filename = name.to_string_lossy().into_owned();
    }
    rec.output = Some(final_path.to_path_buf());
    if let Some(plan) = rec.video.as_mut() {
        plan.step = VideoStep::Done;
    }
}

/// Finish a video whose final file is `final_path`: a joined file waiting
/// under its temporary name is renamed to it, else it is done when the file is
/// there, else its one stream is moved there. A joined video's streams are
/// removed.
fn complete_at(final_path: &Path, stream_files: &[PathBuf]) -> Result<Outcome, Error> {
    let completed = |path: &Path| {
        Ok(Outcome::Completed {
            path: path.to_path_buf(),
            steals: 0,
        })
    };
    let joined = joining_path(final_path);
    if joined.is_file() {
        // The name was free when it was chosen; if it is not now, the other
        // file stays and the video takes the next free name.
        let target = free_final_path(final_path);
        std::fs::rename(&joined, &target)?;
        remove_stream_files(stream_files);
        return completed(&target);
    }
    if final_path.is_file() {
        if stream_files.len() > 1 {
            remove_stream_files(stream_files);
        }
        return completed(final_path);
    }
    match stream_files {
        [only] if only.is_file() => {
            std::fs::rename(only, final_path)?;
            completed(final_path)
        }
        _ => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot find {}", final_path.display()),
        ))),
    }
}

/// Delete the streams of a joined video, with their part and state files.
/// Best effort: a file that stays is only clutter, and is logged.
fn remove_stream_files(stream_files: &[PathBuf]) {
    let files = stream_files
        .iter()
        .flat_map(|file| [file.clone(), part_path(file), state_path(file)]);
    for file in files {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %file.display(), "couldn't remove a stream file: {e}"),
        }
    }
}

/// Get the record ready for this run (ruling R2): the row shows the final name,
/// and counts the streams finished before. A fresh plan's disk check runs first.
fn begin_video_run(record: &Arc<Mutex<TaskRecord>>) -> Result<(), Error> {
    let (check, directory) = {
        let mut guard = record.lock().unwrap();
        let rec = &mut *guard;
        let Some(plan) = rec.video.as_ref().filter(|p| !p.streams.is_empty()) else {
            return Err(no_video());
        };
        let fresh = plan.step == VideoStep::Stream(0) && plan.streams[0].file.is_none();
        let check = fresh
            .then(|| video_space_needed(plan))
            .flatten()
            .map(|needed| (needed, plan.streams.len() > 1));
        let finished = finished_bytes(plan);
        // The resumed stream reports what its state file already covers as
        // downloaded; it was not downloaded by this run, so retries count on.
        let covered = covered_in_current_stream(plan);
        rec.total_bytes = video_total(plan);
        rec.output = Some(rec.directory.join(&rec.filename));
        rec.progress_base = finished;
        rec.downloaded_bytes = finished;
        rec.bytes_at_start = finished + covered;
        (check, rec.directory.clone())
    };
    match check {
        Some((needed, joined)) => check_space(&directory, needed, joined),
        None => Ok(()),
    }
}

/// What to do next, or stop when the user has stopped the download.
fn next_step(record: &Arc<Mutex<TaskRecord>>, cancel: &watch::Receiver<bool>) -> Next {
    let rec = record.lock().unwrap();
    let Some(plan) = rec.video.as_ref() else {
        return Next::Stop;
    };
    if *cancel.borrow() || rec.status != DownloadStatus::Active {
        return Next::Stop;
    }
    match plan.step {
        VideoStep::Stream(index) if index < plan.streams.len() => Next::Stream(index),
        VideoStep::Done => Next::Done(rec.directory.join(&rec.filename)),
        _ => Next::Join,
    }
}

/// Copy out what stream `index` needs, and give the record a fresh progress
/// for it, so the row counts this stream on top of the finished ones.
fn stream_job(record: &Arc<Mutex<TaskRecord>>, index: usize) -> Result<StreamJob, Error> {
    let mut guard = record.lock().unwrap();
    let rec = &mut *guard;
    let plan = rec.video.as_ref().ok_or_else(no_video)?;
    let stream = plan.streams.get(index).cloned().ok_or_else(no_video)?;
    let container = plan.choice.as_ref().map(|c| c.container);
    let container = container.unwrap_or(crate::video::Container::Mp4);
    let file_name = stream_file_name(&rec.filename, container, &stream);
    let progress = fresh_progress(rec);
    rec.progress = Arc::clone(&progress);
    Ok(StreamJob {
        id: rec.id.clone(),
        stream,
        file_name,
        directory: rec.directory.clone(),
        connections: rec.connections,
        referer: rec.referer.clone(),
        user_agent: rec.user_agent.clone(),
        progress,
    })
}

fn fresh_progress(rec: &TaskRecord) -> Arc<Progress> {
    Arc::new(Progress::with_rate_limiters(
        Arc::clone(&rec.share_limiter),
        Arc::clone(&rec.speed_limiter),
    ))
}

/// Bytes of the streams finished before the current step.
fn finished_bytes(plan: &VideoPlan) -> u64 {
    let finished = match plan.step {
        VideoStep::Choosing => 0,
        VideoStep::Stream(index) => index.min(plan.streams.len()),
        VideoStep::Join | VideoStep::Done => plan.streams.len(),
    };
    plan.streams[..finished].iter().map(stream_bytes).sum()
}

/// Bytes the current stream's state file already covers; 0 without one.
fn covered_in_current_stream(plan: &VideoPlan) -> u64 {
    let VideoStep::Stream(index) = plan.step else {
        return 0;
    };
    let Some(file) = plan.streams.get(index).and_then(|s| s.file.as_deref()) else {
        return 0;
    };
    let Ok(state) = crate::state::load(&state_path(file)) else {
        return 0;
    };
    let mut ranges = RangeSet::new();
    for range in state.completed {
        ranges.insert(range);
    }
    ranges.covered()
}

/// A finished stream's bytes: its size, else its file's length.
fn stream_bytes(stream: &StreamPlan) -> u64 {
    let on_disk = || {
        let file = stream.file.as_deref()?;
        std::fs::metadata(file).ok().map(|m| m.len())
    };
    stream.size.or_else(on_disk).unwrap_or(0)
}

/// Refuse to start when the folder's disk lacks room for the whole video.
fn check_space(directory: &Path, needed: u64, joined: bool) -> Result<(), Error> {
    match ensure_disk_space(directory, needed, SAFETY_MARGIN_BYTES) {
        Err(Error::InsufficientDiskSpace {
            required,
            available,
            ..
        }) => Err(Error::Video {
            message: space_message(&volume_label(directory), required, available, joined),
            transient: false,
        }),
        other => other,
    }
}

fn space_message(volume: &str, required: u64, available: u64, joined: bool) -> String {
    let note = if joined { JOINED_NOTE } else { "" };
    format!(
        "Needs {} free on {volume}{note}; {} free.",
        format_bytes(required),
        format_bytes(available)
    )
}

/// `final_path`, or the first free numbered name beside it (`Title (1).mp4`),
/// so nothing is overwritten.
fn free_final_path(final_path: &Path) -> PathBuf {
    free_final_path_avoiding(final_path, &HashSet::new())
}

/// `free_final_path`, also skipping the names in `claimed`.
fn free_final_path_avoiding(final_path: &Path, claimed: &HashSet<PathBuf>) -> PathBuf {
    let folder = final_path.parent().unwrap_or(Path::new(""));
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidate = final_path.to_path_buf();
    let mut n = 0;
    while candidate.exists() || part_path(&candidate).exists() || claimed.contains(&candidate) {
        n += 1;
        candidate = folder.join(collision_name(&name, n));
    }
    candidate
}

fn no_video() -> Error {
    Error::Video {
        message: NO_VIDEO.to_string(),
        transient: false,
    }
}

/// Show the current stream's progress as the row's step: `video 1 of 2 · 37%`.
pub(super) fn refresh_stream_step(task: &mut TaskRecord) {
    // A link refresh shows its own step, or the tool it is fetching.
    let refreshing = task.step.as_deref() == Some(REFRESH_STEP) || task.lookup_progress.is_some();
    if !task.running || refreshing {
        return;
    }
    let Some(plan) = task.video.as_ref() else {
        return;
    };
    let index = match plan.step {
        VideoStep::Stream(index) => index,
        VideoStep::Join => return refresh_join_step(task),
        _ => return,
    };
    let Some(stream) = plan.streams.get(index) else {
        return;
    };
    let downloaded = task.progress.downloaded.load(Ordering::Relaxed);
    let engine_total = task.progress.total.load(Ordering::Relaxed);
    let total = Some(engine_total).filter(|&t| t > 0).or(stream.size);
    let percent = total.map_or(0, |t| downloaded.saturating_mul(100) / t.max(1));
    task.step = Some(step_text(plan, index, percent.min(100) as u8));
}

/// Show how far ffmpeg is as the row's step: `Joining 64%`. While ffmpeg is
/// being fetched the step says so instead.
fn refresh_join_step(task: &mut TaskRecord) {
    let Some(percent) = task
        .join_percent
        .as_ref()
        .filter(|_| task.lookup_progress.is_none())
    else {
        return;
    };
    let percent = Some(percent.load(Ordering::Relaxed)).filter(|&p| p != UNKNOWN_PERCENT);
    task.step = Some(joining_text(percent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_message_says_why_a_join_needs_twice_the_size() {
        assert_eq!(
            space_message("/mnt/data", 4 * 1024 * 1024, 1024 * 1024, true),
            "Needs 4.0 MiB free on /mnt/data (the video is joined from two parts, so it \
             briefly needs about twice its size); 1.0 MiB free."
        );
        assert_eq!(
            space_message("C:", 4 * 1024 * 1024, 1024 * 1024, false),
            "Needs 4.0 MiB free on C:; 1.0 MiB free."
        );
    }

    use crate::daemon::task::MAX_AUTO_RETRIES;
    use crate::probe::Validator;
    use crate::ranges::ByteRange;
    use crate::state::{save_atomic, State, STATE_VERSION};
    use crate::video::{ChosenQuality, Container, StreamKind};

    fn completed_path(outcome: Result<Outcome, Error>) -> PathBuf {
        match outcome {
            Ok(Outcome::Completed { path, .. }) => path,
            other => panic!("not completed: {other:?}"),
        }
    }

    #[test]
    fn a_taken_final_name_gets_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("Clip.f140.m4a");
        std::fs::write(&stream, "new").unwrap();
        std::fs::write(dir.path().join("Clip.m4a"), "old").unwrap();

        let free = free_final_path(&dir.path().join("Clip.m4a"));
        let placed = completed_path(complete_at(&free, std::slice::from_ref(&stream)));

        assert_eq!(placed, dir.path().join("Clip (1).m4a"));
        assert_eq!(std::fs::read_to_string(&placed).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Clip.m4a")).unwrap(),
            "old"
        );
        assert!(!stream.exists());
    }

    #[test]
    fn a_rename_cut_short_by_a_restart_finishes_either_way() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("Clip.f140.m4a");
        let final_path = dir.path().join("Clip.m4a");
        std::fs::write(&stream, "audio").unwrap();

        // Stopped before the rename: the stream is moved now.
        let placed = completed_path(complete_at(&final_path, std::slice::from_ref(&stream)));
        assert_eq!(placed, final_path);
        assert!(!stream.exists());

        // Stopped after it: the final file is there, the stream is gone.
        let again = completed_path(complete_at(&final_path, std::slice::from_ref(&stream)));
        assert_eq!(again, final_path);
        assert_eq!(std::fs::read_to_string(&final_path).unwrap(), "audio");

        // Neither there: a plain failure, never a completed download.
        let missing = dir.path().join("Other.m4a");
        assert!(complete_at(&missing, std::slice::from_ref(&stream)).is_err());
    }

    #[test]
    fn a_joined_file_cut_short_before_its_rename_is_renamed_on_the_restart() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("Clip.mp4");
        let streams = [
            dir.path().join("Clip.f137.mp4"),
            dir.path().join("Clip.f140.m4a"),
        ];
        for stream in &streams {
            std::fs::write(stream, "part").unwrap();
            std::fs::write(part_path(stream), "").unwrap();
        }
        std::fs::write(joining_path(&final_path), "joined").unwrap();

        let placed = completed_path(complete_at(&final_path, &streams));

        assert_eq!(placed, final_path);
        assert_eq!(std::fs::read_to_string(&final_path).unwrap(), "joined");
        assert_eq!(names(dir.path()), ["Clip.mp4"]);
    }

    #[test]
    fn a_file_that_took_the_final_name_meanwhile_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("Clip.mp4");
        std::fs::write(&final_path, "someone else's").unwrap();
        std::fs::write(joining_path(&final_path), "joined").unwrap();

        let placed = completed_path(complete_at(&final_path, &[]));

        assert_eq!(placed, dir.path().join("Clip (1).mp4"));
        assert_eq!(
            std::fs::read_to_string(&final_path).unwrap(),
            "someone else's"
        );
        assert_eq!(std::fs::read_to_string(&placed).unwrap(), "joined");
    }

    #[test]
    fn a_joined_video_found_done_after_its_rename_drops_the_streams_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("Clip.mp4");
        std::fs::write(&final_path, "joined").unwrap();
        let streams = [
            dir.path().join("Clip.f137.mp4"),
            dir.path().join("Clip.f140.m4a"),
        ];
        std::fs::write(&streams[0], "left over").unwrap();

        completed_path(complete_at(&final_path, &streams));

        assert_eq!(names(dir.path()), ["Clip.mp4"]);
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A record half way through its one stream, with a state file covering `covered` bytes.
    fn half_done_record(dir: &Path, covered: u64) -> TaskRecord {
        let file = dir.join("Clip.f140.m4a");
        let state = State {
            version: STATE_VERSION,
            url: "https://video.test/stream".to_string(),
            final_url: "https://video.test/stream".to_string(),
            total: 2 * covered,
            validator: Validator::default(),
            completed: vec![ByteRange::new(0, covered)],
            output: file.clone(),
            connections: Some(1),
        };
        save_atomic(&state_path(&file), &state).unwrap();
        let mut rec = TaskRecord::new(
            "dl_1".to_string(),
            "https://video.test/watch".to_string(),
            dir.to_path_buf(),
            None,
            1,
        );
        rec.filename = "Clip.m4a".to_string();
        rec.video = Some(VideoPlan {
            page_url: "https://video.test/watch".to_string(),
            title: "Clip".to_string(),
            duration_secs: None,
            choice: Some(ChosenQuality {
                label: "Audio only".to_string(),
                container: Container::M4a,
                choice_id: "audio-m4a".to_string(),
            }),
            choices: Vec::new(),
            offered: Vec::new(),
            default: 0,
            streams: vec![StreamPlan {
                format_id: "140".to_string(),
                kind: StreamKind::Audio,
                ext: "m4a".to_string(),
                url: "https://video.test/stream".to_string(),
                size: Some(2 * covered),
                headers: Vec::new(),
                request_bytes: None,
                file: Some(file),
            }],
            step: VideoStep::Stream(0),
            refreshes: Vec::new(),
            site: String::new(),
        });
        rec
    }

    #[test]
    fn bytes_a_resumed_stream_already_had_are_not_progress_of_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let record = Arc::new(Mutex::new(half_done_record(dir.path(), 500_000)));

        begin_video_run(&record).unwrap();

        let mut rec = record.lock().unwrap();
        assert_eq!(rec.progress_base, 0);
        assert_eq!(rec.bytes_at_start, 500_000);
        // The resume reports the covered bytes; the ranges then keep failing.
        rec.progress.downloaded.store(500_000, Ordering::Relaxed);
        rec.status = DownloadStatus::Active;
        rec.retries = MAX_AUTO_RETRIES - 1;
        rec.apply_outcome(Err(Error::Protocol("status 500".into())));
        assert_eq!(rec.retries, MAX_AUTO_RETRIES, "the retries count on");
        rec.status = DownloadStatus::Active;
        rec.apply_outcome(Err(Error::Protocol("status 500".into())));
        assert_eq!(rec.status, DownloadStatus::Failed);
    }
}
