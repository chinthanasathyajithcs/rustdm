//! Running a chosen video: each stream goes through the engine in turn, in
//! requests no larger than the site allows, then the streams become the final
//! file (spec sections 5.3 and 6).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_fields::{step_text, stream_file_name, video_space_needed, video_total};
use crate::disk::{ensure_disk_space, volume_label, SAFETY_MARGIN_BYTES};
use crate::engine::{self, DownloadRequest, Outcome, Progress, ResumeOptions, Timeouts};
use crate::error::Error;
use crate::ipc::protocol::DownloadStatus;
use crate::ranges::RangeSet;
use crate::request_context::{stream_client, RequestContext};
use crate::state::{part_path, state_path};
use crate::template::collision_name;
use crate::tui::format::format_bytes;
use crate::video::errors::NO_VIDEO;
use crate::video::{StreamPlan, VideoPlan, VideoStep};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use url::Url;

/// What a video of two streams ends with until the join arrives.
const JOIN_LATER: &str = "Joining arrives in the next task";
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
        loop {
            match next_step(record, &cancel) {
                Next::Stop => return self.finish_task(record, Err(Error::Cancelled)),
                Next::Stream(index) => match self.run_stream(record, index, cancel.clone()).await {
                    Ok(Outcome::Completed { path, .. }) => self.stream_done(record, index, path),
                    // Task 6 takes `LinkExpired` over: it refreshes the links.
                    other => return self.finish_task(record, other),
                },
                Next::Join => return self.join_streams(record),
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
    /// the final name; two wait for the join (Task 5).
    fn join_streams(&self, record: &Arc<Mutex<TaskRecord>>) {
        let files = stream_files(record);
        let outcome = match files.as_slice() {
            [] => Err(no_video()),
            [_] => {
                // The free name and `Done` are saved before the rename, so a
                // restart in between finds the rename to finish, or finished.
                let final_path = mark_done(record);
                self.persist();
                complete_at(&final_path, &files)
            }
            _ => Err(Error::Video {
                message: JOIN_LATER.to_string(),
                transient: false,
            }),
        };
        self.finish_task(record, outcome);
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

/// Pick the free final name, and record it with the step `Done`. Returns the
/// final path.
fn mark_done(record: &Arc<Mutex<TaskRecord>>) -> PathBuf {
    let mut rec = record.lock().unwrap();
    let final_path = free_final_path(&rec.directory.join(&rec.filename));
    if let Some(name) = final_path.file_name() {
        rec.filename = name.to_string_lossy().into_owned();
    }
    rec.output = Some(final_path.clone());
    if let Some(plan) = rec.video.as_mut() {
        plan.step = VideoStep::Done;
    }
    final_path
}

/// Finish a video whose final file is `final_path`: done when it is there,
/// else its one stream is moved there.
fn complete_at(final_path: &Path, stream_files: &[PathBuf]) -> Result<Outcome, Error> {
    let done = || {
        Ok(Outcome::Completed {
            path: final_path.to_path_buf(),
            steals: 0,
        })
    };
    if final_path.is_file() {
        return done();
    }
    match stream_files {
        [only] if only.is_file() => {
            std::fs::rename(only, final_path)?;
            done()
        }
        _ => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot find {}", final_path.display()),
        ))),
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
    let folder = final_path.parent().unwrap_or(Path::new(""));
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidate = final_path.to_path_buf();
    let mut n = 0;
    while candidate.exists() || part_path(&candidate).exists() {
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
    if !task.running {
        return;
    }
    let Some(plan) = task.video.as_ref() else {
        return;
    };
    let VideoStep::Stream(index) = plan.step else {
        return;
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
