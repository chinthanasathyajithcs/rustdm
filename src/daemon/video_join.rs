//! Making a video's final file once its streams are downloaded: one stream is
//! renamed, two are joined with ffmpeg; and the final name is chosen so that
//! nothing is overwritten (spec section 6, ruling R16).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_fields::joining_text;
use super::video_run::no_video;
use crate::engine::{Outcome, Progress};
use crate::error::Error;
use crate::state::{part_path, state_path};
use crate::template::collision_name;
use crate::tools::{FetchCtx, Freshness, Installed, Tool, ToolError};
use crate::video::errors::join_failed_message;
use crate::video::join::{join, joining_path, JoinFailure, JoinRequest, UNKNOWN_PERCENT};
use crate::video::{Container, StreamKind, VideoStep};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// ffmpeg being fetched while the streams download, so the join need not wait.
pub(super) struct FfmpegFetch {
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
    /// Make the final file of the downloaded streams. One stream is renamed to
    /// the final name; two are joined with ffmpeg.
    pub(super) async fn join_streams(
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
    pub(super) fn start_ffmpeg_fetch(
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

pub(super) fn has_two_streams(record: &Arc<Mutex<TaskRecord>>) -> bool {
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
pub(super) fn stream_files(record: &Arc<Mutex<TaskRecord>>) -> Vec<PathBuf> {
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
pub(super) fn complete_at(final_path: &Path, stream_files: &[PathBuf]) -> Result<Outcome, Error> {
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

/// Show how far ffmpeg is as the row's step: `Joining 64%`. While ffmpeg is
/// being fetched the step says so instead.
pub(super) fn refresh_join_step(task: &mut TaskRecord) {
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
}
