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
                    return self.finish_task(record, Ok(Outcome::Completed { path, steals: 0 }))
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
        let (files, final_path) = {
            let rec = record.lock().unwrap();
            let files: Option<Vec<PathBuf>> = rec
                .video
                .iter()
                .flat_map(|plan| &plan.streams)
                .map(|s| s.file.clone())
                .collect();
            let final_path = rec.directory.join(&rec.filename);
            (files.unwrap_or_default(), final_path)
        };
        let outcome = match files.as_slice() {
            [] => Err(no_video()),
            [only] => place_final(only, &final_path).map(|path| {
                set_step(record, VideoStep::Done);
                Outcome::Completed { path, steals: 0 }
            }),
            _ => Err(Error::Video {
                message: JOIN_LATER.to_string(),
                transient: false,
            }),
        };
        self.finish_task(record, outcome);
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
        rec.total_bytes = video_total(plan);
        rec.output = Some(rec.directory.join(&rec.filename));
        rec.progress_base = finished;
        rec.downloaded_bytes = finished;
        rec.bytes_at_start = finished;
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

fn set_step(record: &Arc<Mutex<TaskRecord>>, step: VideoStep) {
    if let Some(plan) = record.lock().unwrap().video.as_mut() {
        plan.step = step;
    }
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

/// Move `file` to `final_path`, or to the first free numbered name beside it
/// (`Title (1).mp4`); nothing is overwritten.
fn place_final(file: &Path, final_path: &Path) -> Result<PathBuf, Error> {
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
    std::fs::rename(file, &candidate)?;
    Ok(candidate)
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

    #[test]
    fn a_taken_final_name_gets_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("Clip.f140.m4a");
        std::fs::write(&stream, "new").unwrap();
        std::fs::write(dir.path().join("Clip.m4a"), "old").unwrap();

        let placed = place_final(&stream, &dir.path().join("Clip.m4a")).unwrap();

        assert_eq!(placed, dir.path().join("Clip (1).m4a"));
        assert_eq!(std::fs::read_to_string(&placed).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Clip.m4a")).unwrap(),
            "old"
        );
        assert!(!stream.exists());
    }
}
