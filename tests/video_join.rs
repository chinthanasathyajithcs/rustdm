//! A video of two streams is joined into one file with ffmpeg (the stand-in
//! program, which writes its inputs end to end; one test runs the real one).
//!
//! The fixture clips in `tests/fixtures/video/` were made with ffmpeg 7:
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc=duration=1:size=64x48:rate=10 -an -c:v libx264 -pix_fmt yuv420p clip.mp4
//! ffmpeg -f lavfi -i sine=duration=1 -vn -c:a aac -b:a 32k clip.m4a
//! ffmpeg -f lavfi -i testsrc=duration=1:size=64x48:rate=10 -an -c:v libvpx-vp9 -b:v 50k clip.webm
//! ffmpeg -f lavfi -i sine=duration=1 -vn -c:a libopus -b:a 24k clip-audio.webm
//! ```

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, DownloadStatus, DownloadSummary,
};
use rustdm::testing::video::{
    recorded_args, video_page_json, write_script, youtube_like, PageSpec, ToolResponse, ToolScript,
    VideoWorld,
};
use rustdm::testing::{pattern_bytes, Route, TestServer};
use rustdm::tools::{Installed, Tool};
use rustdm::video::join::{join, join_args, joining_path, JoinFailure, JoinRequest};
use rustdm::video::Container;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const VIDEO_BYTES: u64 = 30_000;
const AUDIO_BYTES: u64 = 10_000;
const VIDEO_PATH: &str = "/stream/137";
const AUDIO_PATH: &str = "/stream/140";
/// The 720p video, which holds other bytes than the 1080p one.
const OTHER_VIDEO_PATH: &str = "/stream/136";
const OTHER_VIDEO_BYTE: u8 = 0xAB;
const FIXTURES: &str = "tests/fixtures/video";

struct Setup {
    world: VideoWorld,
    server: TestServer,
    data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

async fn setup() -> Setup {
    let world = VideoWorld::new().await;
    let html =
        Route::ok("<html><body>a page</body></html>").with_header("Content-Type", "text/html");
    let server = TestServer::start_routes(vec![
        ("/watch".to_string(), html.clone()),
        ("/watch2".to_string(), html),
        (
            VIDEO_PATH.to_string(),
            Route::ok(pattern_bytes(VIDEO_BYTES)),
        ),
        (
            AUDIO_PATH.to_string(),
            Route::ok(pattern_bytes(AUDIO_BYTES)),
        ),
        (
            OTHER_VIDEO_PATH.to_string(),
            Route::ok(vec![OTHER_VIDEO_BYTE; VIDEO_BYTES as usize]),
        ),
    ])
    .await;
    let data = tempfile::tempdir().unwrap();
    let core = new_core(&world, data.path());
    Setup {
        world,
        server,
        data,
        folder: tempfile::tempdir().unwrap(),
        core,
    }
}

fn new_core(world: &VideoWorld, data: &Path) -> DaemonCore {
    DaemonCore::with_store(QueueStore::in_dir(data))
        .unwrap()
        .with_tools(Arc::clone(&world.store))
}

impl Setup {
    fn page(&self) -> PageSpec {
        let mut spec = youtube_like(&self.server.url());
        for format in &mut spec.formats {
            let (path, size) = match format.format_id.as_str() {
                "137" => (VIDEO_PATH, VIDEO_BYTES),
                "136" => (OTHER_VIDEO_PATH, VIDEO_BYTES),
                "140" => (AUDIO_PATH, AUDIO_BYTES),
                _ => continue,
            };
            format.url = self.server.url().join(path).unwrap().to_string();
            format.filesize = Some(size);
        }
        spec
    }

    /// Add the 1080p video (streams 137 and 140), found by the stand-in yt-dlp.
    fn add_video(&self) -> DownloadSummary {
        self.add_quality("/watch", "1080p")
    }

    /// Add the page at `path` with `quality`; the stand-in answers every page alike.
    fn add_quality(&self, path: &str, quality: &str) -> DownloadSummary {
        let page = ToolResponse {
            stdout: video_page_json(&self.page()),
            ..ToolResponse::default()
        };
        write_script(
            &self.world.ytdlp.exe,
            &ToolScript {
                responses: vec![page],
            },
        );
        self.core
            .add_download(AddDownloadParams {
                url: self.server.url().join(path).unwrap().to_string(),
                destination: Some(self.folder.path().to_path_buf()),
                filename: None,
                connections: Some(1),
                max_bytes_per_sec: None,
                context: None,
                quality: Some(quality.to_string()),
            })
            .unwrap()
    }

    /// Make the stand-in ffmpeg answer call N with `responses[N]`.
    fn script_ffmpeg(&self, responses: Vec<ToolResponse>) {
        write_script(&self.world.ffmpeg.exe, &ToolScript { responses });
    }

    fn stream_requests(&self) -> usize {
        let requests = self.server.requests();
        requests
            .iter()
            .filter(|r| r.path.starts_with("/stream/"))
            .count()
    }

    fn folder_names(&self) -> Vec<String> {
        names_in(self.folder.path())
    }

    fn joined_bytes() -> Vec<u8> {
        [pattern_bytes(VIDEO_BYTES), pattern_bytes(AUDIO_BYTES)].concat()
    }
}

fn joins() -> ToolResponse {
    ToolResponse {
        join_inputs: true,
        ..ToolResponse::default()
    }
}

fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

async fn wait_for(core: &DaemonCore, id: &str, want: DownloadStatus) -> DownloadSummary {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let summary = core.get_status(id).unwrap();
        if summary.status == want {
            return summary;
        }
        assert!(
            Instant::now() < deadline,
            "{id} is {:?}, wanted {want:?} ({:?})",
            summary.status,
            summary.error_message
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Tick the telemetry until the row's step starts with `prefix`.
async fn wait_for_step(core: &DaemonCore, id: &str, prefix: &str) -> DownloadSummary {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        core.tick_telemetry();
        let summary = core.get_status(id).unwrap();
        if summary
            .step
            .as_deref()
            .is_some_and(|s| s.starts_with(prefix))
        {
            return summary;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never reached {prefix:?}: {:?} {:?} {:?}",
            summary.status,
            summary.step,
            summary.error_message
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn two_streams_end_as_one_file_and_the_parts_are_gone() {
    let s = setup().await;

    let added = s.add_video();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.filename, "A test video.mp4");
    assert_eq!(s.folder_names(), ["A test video.mp4"]);
    assert_eq!(
        std::fs::read(s.folder.path().join("A test video.mp4")).unwrap(),
        Setup::joined_bytes()
    );
    let folder = s.folder.path();
    let expected = join_args(
        &folder.join("A test video.f137.mp4"),
        &folder.join("A test video.f140.m4a"),
        Container::Mp4,
        &folder.join("A test video.mp4.joining"),
    );
    let expected: Vec<String> = expected
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(recorded_args(&s.world.ffmpeg.exe), [expected]);
}

#[tokio::test]
async fn a_failed_join_keeps_both_streams_and_r_joins_again() {
    let s = setup().await;
    s.script_ffmpeg(vec![
        ToolResponse {
            stderr: "x\nInvalid data found\n".to_string(),
            exit_code: 1,
            ..joins()
        },
        joins(),
    ]);

    let added = s.add_video();
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    let folder = s.folder.path().display();
    assert_eq!(
        failed.error_message,
        Some(format!(
            "Couldn't join the video and audio: Invalid data found. \
             The two parts are kept in {folder}; press r to try again."
        ))
    );
    assert_eq!(
        s.folder_names(),
        ["A test video.f137.mp4", "A test video.f140.m4a"]
    );
    let requests = s.stream_requests();

    s.core.resume_download(&added.id).unwrap();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.filename, "A test video.mp4");
    assert_eq!(s.folder_names(), ["A test video.mp4"]);
    assert_eq!(s.stream_requests(), requests, "no stream was fetched again");
}

#[tokio::test]
async fn pausing_during_the_join_resumes_with_only_the_join() {
    let s = setup().await;
    s.script_ffmpeg(vec![
        ToolResponse {
            sleep_ms: 3000,
            ..joins()
        },
        joins(),
    ]);

    let added = s.add_video();
    wait_for_step(&s.core, &added.id, "Joining").await;
    let requests = s.stream_requests();
    s.core.pause_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Paused).await;

    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);
    assert_eq!(
        s.folder_names(),
        ["A test video.f137.mp4", "A test video.f140.m4a"]
    );

    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.folder_names(), ["A test video.mp4"]);
    assert_eq!(s.stream_requests(), requests, "no stream was fetched again");
}

#[tokio::test]
async fn the_row_shows_how_far_the_join_is() {
    let s = setup().await;
    // 106 s of the page's 212 s.
    let progress = "out_time_us=N/A\nout_time_us=106000000\nprogress=continue\n";
    s.script_ffmpeg(vec![ToolResponse {
        stdout: progress.to_string(),
        sleep_ms: 3000,
        ..joins()
    }]);

    let added = s.add_video();
    let row = wait_for_step(&s.core, &added.id, "Joining 50%").await;
    s.core
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: true,
        })
        .unwrap();

    assert_eq!(row.step.as_deref(), Some("Joining 50%"));
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);
    assert!(s.folder_names().is_empty(), "{:?}", s.folder_names());
}

#[tokio::test]
async fn two_videos_with_one_name_joining_together_both_survive() {
    let s = setup().await;
    s.script_ffmpeg(vec![ToolResponse {
        sleep_ms: 1500,
        ..joins()
    }]);

    // Both are called "A test video.mp4"; their video streams differ.
    let first = s.add_quality("/watch", "1080p");
    wait_for_step(&s.core, &first.id, "Joining").await;
    let second = s.add_quality("/watch2", "720");
    wait_for_step(&s.core, &second.id, "Joining").await;
    let first_done = wait_for(&s.core, &first.id, DownloadStatus::Completed).await;
    let second_done = wait_for(&s.core, &second.id, DownloadStatus::Completed).await;

    assert_eq!(first_done.filename, "A test video.mp4");
    assert_eq!(second_done.filename, "A test video (1).mp4");
    assert_eq!(
        s.folder_names(),
        ["A test video (1).mp4", "A test video.mp4"]
    );
    let audio = pattern_bytes(AUDIO_BYTES);
    let other_video = vec![OTHER_VIDEO_BYTE; VIDEO_BYTES as usize];
    assert_eq!(
        std::fs::read(s.folder.path().join("A test video.mp4")).unwrap(),
        Setup::joined_bytes()
    );
    assert_eq!(
        std::fs::read(s.folder.path().join("A test video (1).mp4")).unwrap(),
        [other_video, audio].concat()
    );
}

#[tokio::test]
async fn a_restart_during_the_join_comes_back_to_the_join() {
    let s = setup().await;
    s.script_ffmpeg(vec![
        ToolResponse {
            sleep_ms: 3000,
            ..joins()
        },
        joins(),
    ]);

    let added = s.add_video();
    wait_for_step(&s.core, &added.id, "Joining").await;
    let requests = s.stream_requests();
    s.core.shutdown();
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);

    let again = new_core(&s.world, s.data.path());
    again.schedule();
    let done = wait_for(&again, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.filename, "A test video.mp4");
    assert_eq!(s.folder_names(), ["A test video.mp4"]);
    assert_eq!(
        std::fs::read(s.folder.path().join("A test video.mp4")).unwrap(),
        Setup::joined_bytes()
    );
    assert_eq!(s.stream_requests(), requests, "no stream was fetched again");
}

#[tokio::test]
async fn a_taken_name_gets_a_number_and_nothing_is_overwritten() {
    let s = setup().await;
    std::fs::write(s.folder.path().join("A test video.mp4"), "old").unwrap();

    let added = s.add_video();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.filename, "A test video (1).mp4");
    assert_eq!(
        s.folder_names(),
        ["A test video (1).mp4", "A test video.mp4"]
    );
    assert_eq!(
        std::fs::read_to_string(s.folder.path().join("A test video.mp4")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read(s.folder.path().join("A test video (1).mp4")).unwrap(),
        Setup::joined_bytes()
    );
}

/// A system ffmpeg and ffprobe, or None (the test then has nothing to run).
fn real_ffmpeg() -> Option<(Installed, PathBuf)> {
    let find = |name: &str| {
        let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&exe))
            .find(|path| path.is_file())
    };
    let ffmpeg = find("ffmpeg")?;
    let ffprobe = find("ffprobe")?;
    let installed = Installed {
        tool: Tool::Ffmpeg,
        version: "system".to_string(),
        dir: ffmpeg.parent()?.to_path_buf(),
        exe: ffmpeg,
    };
    Some((installed, ffprobe))
}

/// The stream types ffprobe finds in `file`, sorted.
fn stream_types(ffprobe: &Path, file: &Path) -> Vec<String> {
    let output = std::process::Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe failed on {file:?}");
    let mut types: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .collect();
    types.sort();
    types
}

fn clip(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join(name)
}

async fn join_clips(ffmpeg: &Installed, video: &Path, audio: &Path, c: Container, temp: &Path) {
    let percent = AtomicU8::new(u8::MAX);
    let (_keep, cancel) = watch::channel(false);
    let request = JoinRequest {
        video,
        audio,
        container: c,
        temp,
        duration_secs: Some(1),
        percent: &percent,
        cancel,
    };
    join(ffmpeg, request).await.expect("the clips join");
}

#[tokio::test]
async fn the_real_ffmpeg_joins_the_clips() {
    let Some((ffmpeg, ffprobe)) = real_ffmpeg() else {
        eprintln!("skipped: ffmpeg and ffprobe are not on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    let mp4 = joining_path(&dir.path().join("out.mp4"));
    join_clips(
        &ffmpeg,
        &clip("clip.mp4"),
        &clip("clip.m4a"),
        Container::Mp4,
        &mp4,
    )
    .await;
    let webm = joining_path(&dir.path().join("out.webm"));
    join_clips(
        &ffmpeg,
        &clip("clip.webm"),
        &clip("clip-audio.webm"),
        Container::Webm,
        &webm,
    )
    .await;

    assert_eq!(stream_types(&ffprobe, &mp4), ["audio", "video"]);
    assert_eq!(stream_types(&ffprobe, &webm), ["audio", "video"]);
}

#[tokio::test]
async fn the_real_ffmpeg_failing_says_why_and_leaves_no_file() {
    let Some((ffmpeg, _ffprobe)) = real_ffmpeg() else {
        eprintln!("skipped: ffmpeg and ffprobe are not on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let not_media = dir.path().join("not-media.mp4");
    std::fs::write(&not_media, "this is not a video").unwrap();
    let temp = dir.path().join("out.mp4.joining");
    let percent = AtomicU8::new(u8::MAX);
    let (_keep, cancel) = watch::channel(false);
    let request = JoinRequest {
        video: &not_media,
        audio: &clip("clip.m4a"),
        container: Container::Mp4,
        temp: &temp,
        duration_secs: None,
        percent: &percent,
        cancel,
    };

    let failure = join(&ffmpeg, request).await.unwrap_err();

    let JoinFailure::Exited { last_line } = failure else {
        panic!("not an exit: {failure:?}");
    };
    assert!(!last_line.is_empty());
    assert!(!temp.exists());
}
