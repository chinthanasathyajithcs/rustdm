//! Joins a video's two streams into one file with ffmpeg, without
//! re-encoding: the video of one stream and the audio of the other are copied
//! side by side into a temporary file next to the final one.

use super::ytdlp::read_tail;
use super::Container;
use crate::tools::Installed;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// What `JoinRequest::percent` holds until ffmpeg has reported how far it is.
pub const UNKNOWN_PERCENT: u8 = u8::MAX;

/// What to join, and where to watch and stop it.
pub struct JoinRequest<'a> {
    pub video: &'a Path,
    /// None: `video` alone is copied into the container (a remux).
    pub audio: Option<&'a Path>,
    pub container: Container,
    /// Where ffmpeg writes; removed when the join is stopped or fails.
    pub temp: &'a Path,
    /// How long the video runs, to turn ffmpeg's progress into a percent.
    pub duration_secs: Option<u64>,
    /// Updated as ffmpeg reports its progress; `UNKNOWN_PERCENT` until it does.
    pub percent: &'a AtomicU8,
    pub cancel: watch::Receiver<bool>,
}

/// Why a join did not produce a file.
#[derive(Debug, PartialEq, Eq)]
pub enum JoinFailure {
    /// The user paused or cancelled: ffmpeg was stopped.
    Cancelled,
    /// ffmpeg ran and failed; `last_line` is the last thing it said.
    Exited { last_line: String },
    /// ffmpeg could not be started or watched.
    Spawn(String),
}

/// Ending of the temporary file, which is renamed to the final name when done.
const JOINING_SUFFIX: &str = ".joining";
/// The longest error line shown to the user, in characters.
const MAX_ERROR_LINE_CHARS: usize = 200;
/// The last part of ffmpeg's error output that is kept.
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// How long to wait for the output after ffmpeg has exited.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
const MICROS_PER_SEC: u64 = 1_000_000;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The temporary file ffmpeg writes before it is renamed to `final_path`.
pub fn joining_path(final_path: &Path) -> PathBuf {
    let mut temp = final_path.as_os_str().to_os_string();
    temp.push(JOINING_SUFFIX);
    PathBuf::from(temp)
}

/// ffmpeg's command line: copy the video of `video` and the audio of `audio`
/// into `temp`, with progress on stdout; without `audio`, the video and audio
/// tracks of `video` (a remux). Each argument stands alone; no shell is
/// involved. Only an MP4 is made to start playing before it is fully loaded.
pub fn join_args(
    video: &Path,
    audio: Option<&Path>,
    container: Container,
    temp: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-progress",
        "pipe:1",
        "-i",
    ]
    .map(OsString::from)
    .into();
    args.push(video.into());
    let maps = match audio {
        Some(audio) => {
            args.push("-i".into());
            args.push(audio.into());
            ["-map", "0:v:0", "-map", "1:a:0"]
        }
        None => ["-map", "0:v?", "-map", "0:a?"],
    };
    args.extend(maps.map(OsString::from));
    args.extend(["-c", "copy"].map(OsString::from));
    let format = match container {
        Container::Webm => "webm",
        Container::Mp4 | Container::M4a => {
            args.extend(["-movflags", "+faststart"].map(OsString::from));
            "mp4"
        }
    };
    args.extend(["-f", format].map(OsString::from));
    args.push(temp.into());
    args
}

/// How far a join is, from one line of ffmpeg's `-progress` output
/// (`out_time_us=<microseconds done>`); none for any other line, an
/// unknown position, or when the video's length is not known.
pub fn join_percent(line: &str, duration_secs: Option<u64>) -> Option<u8> {
    let done: u64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    let total = duration_secs?
        .checked_mul(MICROS_PER_SEC)
        .filter(|&t| t > 0)?;
    let percent = u128::from(done) * 100 / u128::from(total);
    Some(percent.min(100) as u8)
}

/// The last non-empty line of ffmpeg's error output, at most 200 characters.
pub fn last_error_line(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    line.chars().take(MAX_ERROR_LINE_CHARS).collect()
}

/// Run ffmpeg to join the two streams into `req.temp`. When it is stopped or
/// fails, ffmpeg is gone and `req.temp` is removed.
pub async fn join(ffmpeg: &Installed, req: JoinRequest<'_>) -> Result<(), JoinFailure> {
    // A file an earlier, interrupted join left behind is not ours to build on.
    remove_temp(req.temp);
    let args = join_args(req.video, req.audio, req.container, req.temp);
    let mut child = spawn_ffmpeg(&ffmpeg.exe, &args).map_err(spawn_failure)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let duration = req.duration_secs;
    let percent = req.percent;
    let stderr_task = tokio::spawn(read_tail(stderr, MAX_STDERR_BYTES));

    // Progress is read while ffmpeg runs; it ends when ffmpeg's output does.
    let running = async {
        let (_, status) = tokio::join!(watch_progress(stdout, duration, percent), child.wait());
        status
    };
    let waited = tokio::select! {
        status = running => Some(status),
        () = until_cancelled(req.cancel) => None,
    };
    let failure = match waited {
        None => {
            if let Err(e) = child.kill().await {
                tracing::warn!("couldn't stop ffmpeg: {e}");
            }
            Some(JoinFailure::Cancelled)
        }
        Some(Err(e)) => Some(spawn_failure(e)),
        Some(Ok(status)) if status.success() => None,
        Some(Ok(status)) => {
            let stderr = finished_text(stderr_task).await;
            Some(JoinFailure::Exited {
                last_line: exit_line(&stderr, status.code()),
            })
        }
    };
    match failure {
        None => Ok(()),
        Some(failure) => {
            remove_temp(req.temp);
            Err(failure)
        }
    }
}

/// A failure to start or watch ffmpeg, with the same length limit as what it says.
fn spawn_failure(error: std::io::Error) -> JoinFailure {
    JoinFailure::Spawn(last_error_line(&error.to_string()))
}

fn spawn_ffmpeg(exe: &Path, args: &[OsString]) -> std::io::Result<Child> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.spawn()
}

/// Resolves when the flag turns true; never when its sender is gone.
async fn until_cancelled(mut cancel: watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Keep `percent` as far along as ffmpeg's progress lines say.
async fn watch_progress(stdout: impl AsyncRead + Unpin, duration: Option<u64>, percent: &AtomicU8) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(now) = join_percent(&line, duration) {
            percent.store(now, Ordering::Relaxed);
        }
    }
}

/// The text a finished ffmpeg left on its error output.
async fn finished_text(task: tokio::task::JoinHandle<String>) -> String {
    let mut task = task;
    match tokio::time::timeout(OUTPUT_GRACE, &mut task).await {
        Ok(read) => read.unwrap_or_default(),
        Err(_grace_over) => {
            task.abort();
            String::new()
        }
    }
}

/// What to tell the user ffmpeg said; its exit code when it said nothing.
fn exit_line(stderr: &str, code: Option<i32>) -> String {
    let line = last_error_line(stderr);
    if !line.is_empty() {
        return line;
    }
    match code {
        Some(code) => format!("ffmpeg stopped with code {code}"),
        None => "ffmpeg was stopped".to_string(),
    }
}

fn remove_temp(temp: &Path) {
    match std::fs::remove_file(temp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %temp.display(), "couldn't remove the joining file: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn mp4_args_are_exact() {
        let args = join_args(
            Path::new("a.f137.mp4"),
            Some(Path::new("a.f140.m4a")),
            Container::Mp4,
            Path::new("a.mp4.joining"),
        );

        assert_eq!(
            strings(args),
            [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-progress",
                "pipe:1",
                "-i",
                "a.f137.mp4",
                "-i",
                "a.f140.m4a",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "a.mp4.joining",
            ]
        );
    }

    #[test]
    fn one_stream_is_remuxed_with_its_video_and_audio() {
        let args = join_args(
            Path::new("a.fhls-720.mp4"),
            None,
            Container::Mp4,
            Path::new("a.mp4.joining"),
        );

        assert_eq!(
            strings(args)[6..],
            [
                "-i",
                "a.fhls-720.mp4",
                "-map",
                "0:v?",
                "-map",
                "0:a?",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "a.mp4.joining",
            ]
        );
    }

    #[test]
    fn webm_args_have_no_faststart() {
        let args = strings(join_args(
            Path::new("a.f248.webm"),
            Some(Path::new("a.f251.webm")),
            Container::Webm,
            Path::new("a.webm.joining"),
        ));

        assert!(!args.contains(&"-movflags".to_string()), "{args:?}");
        assert!(!args.contains(&"+faststart".to_string()), "{args:?}");
        assert_eq!(&args[args.len() - 3..], ["-f", "webm", "a.webm.joining"]);
    }

    #[test]
    fn the_temporary_file_is_the_final_name_plus_joining() {
        assert_eq!(
            joining_path(Path::new("/v/A video.mp4")),
            PathBuf::from("/v/A video.mp4.joining")
        );
    }

    #[test]
    fn progress_lines_give_a_percent() {
        assert_eq!(join_percent("out_time_us=10600000", Some(212)), Some(5));
        assert_eq!(join_percent("out_time_us=N/A", Some(212)), None);
        assert_eq!(
            join_percent("out_time_us=999999999999", Some(212)),
            Some(100)
        );
        assert_eq!(join_percent("out_time_us=10600000", None), None);
        assert_eq!(join_percent("out_time_us=10600000", Some(0)), None);
        assert_eq!(join_percent("frame=12", Some(212)), None);
        assert_eq!(join_percent("out_time_us=-5", Some(212)), None);
    }

    #[test]
    fn the_last_error_line_is_the_last_one_with_text() {
        assert_eq!(
            last_error_line("x\nInvalid data found\n \n"),
            "Invalid data found"
        );
        assert_eq!(last_error_line(""), "");
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_short_spawn_failure() {
        let missing = Installed {
            tool: crate::tools::Tool::Ffmpeg,
            version: "none".to_string(),
            dir: PathBuf::from("/no/such/folder"),
            exe: PathBuf::from("/no/such/folder/ffmpeg"),
        };
        let percent = AtomicU8::new(UNKNOWN_PERCENT);
        let (_keep, cancel) = watch::channel(false);
        let request = JoinRequest {
            video: Path::new("v.mp4"),
            audio: Some(Path::new("a.m4a")),
            container: Container::Mp4,
            temp: Path::new("/no/such/folder/out.joining"),
            duration_secs: None,
            percent: &percent,
            cancel,
        };

        let failure = join(&missing, request).await.unwrap_err();

        let JoinFailure::Spawn(text) = failure else {
            panic!("not a spawn failure: {failure:?}");
        };
        assert!(!text.is_empty() && text.chars().count() <= 200, "{text}");
    }

    #[test]
    fn the_last_error_line_is_trimmed_to_200_characters() {
        let long = "é".repeat(500);

        let line = last_error_line(&format!("first\n{long}\n"));

        assert_eq!(line.chars().count(), 200);
    }
}
