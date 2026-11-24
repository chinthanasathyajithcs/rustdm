//! Running a chosen video: each stream goes through the engine in turn, in
//! requests no larger than the site allows, then the streams become the final
//! file (spec sections 5.3 and 6).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_fields::{
    pieces_step_text, step_text, stream_file_name, video_space_needed, video_total,
};
use super::video_join::{complete_at, needs_ffmpeg, refresh_join_step, stream_files, FfmpegFetch};
use super::video_refresh::{link_expired_while_running, video_changed, Refreshed};
use crate::disk::{ensure_disk_space, volume_label, SAFETY_MARGIN_BYTES};
use crate::engine::{self, DownloadRequest, Outcome, Progress, ResumeOptions, Timeouts};
use crate::error::Error;
use crate::ipc::protocol::DownloadStatus;
use crate::ranges::RangeSet;
use crate::request_context::{stream_client, RequestContext};
use crate::state::{part_path, state_path};
use crate::tui::format::format_bytes;
use crate::video::errors::NO_VIDEO;
use crate::video::refresh::REFRESH_STEP;
use crate::video::{StreamPlan, VideoPlan, VideoStep};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
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

/// What a stream's run needs, copied out so no lock is held while it runs.
pub(super) struct StreamJob {
    pub(super) id: String,
    pub(super) stream: StreamPlan,
    pub(super) file_name: String,
    pub(super) directory: PathBuf,
    pub(super) connections: u32,
    pub(super) referer: Option<String>,
    pub(super) user_agent: Option<String>,
    pub(super) progress: Arc<Progress>,
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
                    if index == 0 && ffmpeg.is_none() && needs_ffmpeg(record) {
                        ffmpeg = self.start_ffmpeg_fetch(record, &cancel).ok();
                    }
                    let outcome = if is_pieces_stream(record, index) {
                        self.run_pieces_stream(record, index, cancel.clone()).await
                    } else {
                        self.run_stream(record, index, cancel.clone()).await
                    };
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
}

/// Whether stream `index` comes in pieces (HLS or DASH).
fn is_pieces_stream(record: &Arc<Mutex<TaskRecord>>, index: usize) -> bool {
    let rec = record.lock().unwrap();
    let stream = rec.video.as_ref().and_then(|plan| plan.streams.get(index));
    stream.is_some_and(|s| s.source.is_pieces())
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
pub(super) fn stream_job(
    record: &Arc<Mutex<TaskRecord>>,
    index: usize,
) -> Result<StreamJob, Error> {
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

/// A finished stream's bytes: its size, else its file's length. A pieces
/// stream's size is an estimate, so its file's length comes first.
fn stream_bytes(stream: &StreamPlan) -> u64 {
    let on_disk = || {
        let file = stream.file.as_deref()?;
        std::fs::metadata(file).ok().map(|m| m.len())
    };
    if stream.source.is_pieces() {
        return on_disk().or(stream.size).unwrap_or(0);
    }
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

pub(super) fn no_video() -> Error {
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
    if let Some(counter) = &task.pieces_counter {
        let done = counter.done.load(Ordering::Relaxed);
        let total = counter.total.load(Ordering::Relaxed);
        task.step = Some(pieces_step_text(plan, index, done, total));
        return;
    }
    let downloaded = task.progress.downloaded.load(Ordering::Relaxed);
    let engine_total = task.progress.total.load(Ordering::Relaxed);
    let total = Some(engine_total).filter(|&t| t > 0).or(stream.size);
    let percent = total.map_or(0, |t| downloaded.saturating_mul(100) / t.max(1));
    task.step = Some(step_text(plan, index, percent.min(100) as u8));
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
                source: crate::video::StreamSource::Direct,
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
