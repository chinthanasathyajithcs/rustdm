//! A video stream whose link expires gets a fresh one from yt-dlp and goes on
//! where it stopped (spec 6.4).

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{AddDownloadParams, DownloadStatus, DownloadSummary};
use rustdm::request_context::{ContextCookie, RequestContext};
use rustdm::testing::video::{
    recorded_args, recorded_cookies, video_page_json, write_script, youtube_like, PageSpec,
    ToolResponse, ToolScript, VideoWorld,
};
use rustdm::testing::{pattern_bytes, Route, TestServer};
use rustdm::video::refresh::VIDEO_CHANGED;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const VIDEO_BYTES: u64 = 2_000_000;
const AUDIO_BYTES: u64 = 300_000;
const CHUNK: u64 = 500_000;
const VIDEO_PATH: &str = "/stream/137";
/// Where the refreshed link of the video points.
const FRESH_VIDEO_PATH: &str = "/stream/137b";
const AUDIO_PATH: &str = "/stream/140";
/// Slow enough per request that a test can act part-way through the video.
const SLOW_BYTES_PER_SEC: u64 = 400_000;
/// How far the video gets before its link is made to expire.
const EXPIRE_AFTER: u64 = 600_000;
const LIMIT_MESSAGE: &str =
    "YouTube keeps refusing this video's links; press r to try again later.";

struct Setup {
    world: VideoWorld,
    server: TestServer,
    data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

async fn setup(video: Route) -> Setup {
    let world = VideoWorld::new().await;
    let page = Route::ok("<html>a page</html>").with_header("Content-Type", "text/html");
    let server = TestServer::start_routes(vec![
        ("/watch".to_string(), page),
        (VIDEO_PATH.to_string(), video),
        (
            FRESH_VIDEO_PATH.to_string(),
            Route::ok(pattern_bytes(VIDEO_BYTES)),
        ),
        (
            AUDIO_PATH.to_string(),
            Route::ok(pattern_bytes(AUDIO_BYTES)),
        ),
    ])
    .await;
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(Arc::clone(&world.store));
    Setup {
        world,
        server,
        data,
        folder,
        core,
    }
}

fn slow_video() -> Route {
    Route::ok(pattern_bytes(VIDEO_BYTES)).throttled(SLOW_BYTES_PER_SEC)
}

impl Setup {
    /// The YouTube-like page with the video at `video_path` and the given size.
    fn page(&self, video_path: &str, video_size: u64) -> PageSpec {
        let mut spec = youtube_like(&self.server.url());
        for format in &mut spec.formats {
            if format.http_chunk_size.is_some() {
                format.http_chunk_size = Some(CHUNK);
            }
            let (path, size) = match format.format_id.as_str() {
                "137" => (video_path, video_size),
                "140" => (AUDIO_PATH, AUDIO_BYTES),
                _ => continue,
            };
            format.url = self.server.url().join(path).unwrap().to_string();
            format.filesize = Some(size);
        }
        spec
    }

    /// yt-dlp answers with these pages in turn, the last one from then on.
    fn answers(&self, pages: &[PageSpec]) {
        let responses = pages
            .iter()
            .map(|spec| ToolResponse {
                stdout: video_page_json(spec),
                ..ToolResponse::default()
            })
            .collect();
        write_script(&self.world.ytdlp.exe, &ToolScript { responses });
    }

    fn add(&self, context: Option<RequestContext>) -> DownloadSummary {
        self.core
            .add_download(AddDownloadParams {
                url: self.server.url().join("/watch").unwrap().to_string(),
                destination: Some(self.folder.path().to_path_buf()),
                filename: None,
                connections: Some(2),
                max_bytes_per_sec: None,
                context,
                quality: Some("1080p".to_string()),
                added_by: None,
            })
            .unwrap()
    }

    fn lookups(&self) -> usize {
        recorded_args(&self.world.ytdlp.exe).len()
    }

    /// Bytes asked of `path`, the size probe left out.
    fn bytes_asked(&self, path: &str) -> u64 {
        self.server
            .requests()
            .iter()
            .filter(|r| r.path == path)
            .filter_map(|r| r.range.as_deref())
            .filter(|range| *range != "bytes=0-0")
            .map(|range| {
                let (start, end) = range
                    .strip_prefix("bytes=")
                    .and_then(|b| b.split_once('-'))
                    .expect("a bytes=a-b range");
                end.parse::<u64>().unwrap() - start.parse::<u64>().unwrap() + 1
            })
            .sum()
    }

    /// Wait until the video has `bytes`, then make its link answer 403.
    async fn expire_after(&self, id: &str, bytes: u64) {
        wait_until(&self.core, id, |s| s.downloaded_bytes >= bytes).await;
        self.server.set_route(VIDEO_PATH, Route::status(403));
    }
}

fn joined_bytes() -> Vec<u8> {
    [pattern_bytes(VIDEO_BYTES), pattern_bytes(AUDIO_BYTES)].concat()
}

fn names_in(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn cookie_context() -> RequestContext {
    RequestContext {
        cookies: vec![ContextCookie {
            name: "sid".to_string(),
            value: "abc".to_string(),
            domain: "127.0.0.1".to_string(),
            path: "/".to_string(),
            secure: false,
            host_only: true,
            expires: None,
        }],
        referer: None,
        user_agent: None,
    }
}

/// Tick the telemetry until `wanted` holds for the row; returns that row.
async fn wait_until(
    core: &DaemonCore,
    id: &str,
    wanted: impl Fn(&DownloadSummary) -> bool,
) -> DownloadSummary {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        core.tick_telemetry();
        let summary = core.get_status(id).unwrap();
        if wanted(&summary) {
            return summary;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never got there: {:?} {:?} {:?}",
            summary.status,
            summary.step,
            summary.error_message
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for(core: &DaemonCore, id: &str, want: DownloadStatus) -> DownloadSummary {
    wait_until(core, id, |s| s.status == want).await
}

#[tokio::test]
async fn a_403_mid_stream_refreshes_and_continues_without_starting_over() {
    let s = setup(slow_video()).await;
    s.answers(&[
        s.page(VIDEO_PATH, VIDEO_BYTES),
        s.page(FRESH_VIDEO_PATH, VIDEO_BYTES),
    ]);

    let added = s.add(None);
    s.expire_after(&added.id, EXPIRE_AFTER).await;
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.lookups(), 2);
    let asked = s.bytes_asked(VIDEO_PATH) + s.bytes_asked(FRESH_VIDEO_PATH);
    assert!(asked < 2 * VIDEO_BYTES, "asked for {asked} bytes");
    assert!(s.bytes_asked(FRESH_VIDEO_PATH) > 0);
    assert_eq!(std::fs::read(&done.destination).unwrap(), joined_bytes());
}

#[tokio::test]
async fn the_fourth_refresh_in_ten_minutes_pauses_and_r_refreshes_again() {
    let s = setup(Route::status(403)).await;
    s.answers(&[s.page(VIDEO_PATH, VIDEO_BYTES)]);

    let added = s.add(None);
    let paused = wait_for(&s.core, &added.id, DownloadStatus::Paused).await;

    assert_eq!(paused.error_message.as_deref(), Some(LIMIT_MESSAGE));
    // The lookup, then three refreshes.
    assert_eq!(s.lookups(), 4);

    s.answers(&[s.page(FRESH_VIDEO_PATH, VIDEO_BYTES)]);
    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(
        s.lookups(),
        1,
        "r cleared the limit, so the link was refreshed"
    );
}

#[tokio::test]
async fn a_changed_size_on_refresh_fails_with_the_video_changed_message() {
    let s = setup(slow_video()).await;
    s.answers(&[
        s.page(VIDEO_PATH, VIDEO_BYTES),
        s.page(FRESH_VIDEO_PATH, VIDEO_BYTES + 1),
    ]);

    let added = s.add(None);
    s.expire_after(&added.id, EXPIRE_AFTER).await;
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(VIDEO_CHANGED));
    let names = names_in(s.folder.path());
    assert!(
        names.iter().any(|n| n.starts_with("A test video.f137.mp4")),
        "the stream file is kept: {names:?}"
    );
}

#[tokio::test]
async fn a_paused_video_whose_link_expired_refreshes_before_resuming() {
    let s = setup(slow_video()).await;
    s.answers(&[
        s.page(VIDEO_PATH, VIDEO_BYTES),
        s.page(FRESH_VIDEO_PATH, VIDEO_BYTES),
    ]);
    let added = s.add(None);
    wait_until(&s.core, &added.id, |r| r.downloaded_bytes >= EXPIRE_AFTER).await;

    s.core.pause_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Paused).await;
    s.server.set_route(VIDEO_PATH, Route::status(403));
    s.core.resume_download(&added.id).unwrap();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.lookups(), 2);
    assert_eq!(std::fs::read(&done.destination).unwrap(), joined_bytes());
}

#[tokio::test]
async fn a_refresh_keeps_the_sign_in() {
    let s = setup(slow_video()).await;
    s.answers(&[
        s.page(VIDEO_PATH, VIDEO_BYTES),
        s.page(FRESH_VIDEO_PATH, VIDEO_BYTES),
    ]);
    // The fresh link is slow too, so the download is still running (and
    // holds its sign-in) when the test looks, even in a slow build.
    s.server.set_route(FRESH_VIDEO_PATH, slow_video());
    let added = s.add(Some(cookie_context()));
    s.expire_after(&added.id, EXPIRE_AFTER).await;

    let secret = s
        .data
        .path()
        .join("secrets")
        .join(format!("{}.cookies", added.id));
    wait_until(&s.core, &added.id, |_| s.lookups() == 2).await;
    assert!(secret.exists(), "the sign-in was kept for the refresh");
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    let seen = recorded_cookies(&s.world.ytdlp.exe);
    assert_eq!(seen.len(), 2, "both lookups carried the cookies");
    assert!(seen[1].content.contains("sid\tabc"), "{}", seen[1].content);
}
