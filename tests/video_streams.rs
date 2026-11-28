//! A chosen video downloads its streams, one after the other, through the
//! engine (the stand-in yt-dlp gives the page; a test server gives the streams).

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::video_fields::video_space_needed;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, ChooseQualityParams, DownloadStatus, DownloadSummary,
};
use rustdm::testing::video::{
    video_page_json, write_script, youtube_like, PageSpec, ToolResponse, ToolScript, VideoWorld,
};
use rustdm::testing::{pattern_bytes, RequestLog, Route, TestServer};
use rustdm::video::{StreamKind, StreamPlan, StreamSource, VideoPlan, VideoStep};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const VIDEO_BYTES: u64 = 2_000_000;
const AUDIO_BYTES: u64 = 300_000;
/// The `http_chunk_size` the page asks for.
const CHUNK: u64 = 500_000;
const VIDEO_PATH: &str = "/stream/137";
const AUDIO_PATH: &str = "/stream/140";
/// The engine's size probe, which is not part of the file.
const PROBE_RANGE: &str = "bytes=0-0";

struct Setup {
    world: VideoWorld,
    server: TestServer,
    data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

fn html_page() -> Route {
    Route::ok("<html><body>a page</body></html>").with_header("Content-Type", "text/html")
}

/// A world whose stream server answers the video with `video` and the audio with `audio`.
async fn setup_with(video: Route, audio: Route) -> Setup {
    let world = VideoWorld::new().await;
    let server = TestServer::start_routes(vec![
        ("/watch".to_string(), html_page()),
        (VIDEO_PATH.to_string(), video),
        (AUDIO_PATH.to_string(), audio),
    ])
    .await;
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = new_core(&world, data.path());
    Setup {
        world,
        server,
        data,
        folder,
        core,
    }
}

async fn setup() -> Setup {
    setup_with(video_route(), audio_route()).await
}

fn video_route() -> Route {
    Route::ok(pattern_bytes(VIDEO_BYTES))
}

fn audio_route() -> Route {
    Route::ok(pattern_bytes(AUDIO_BYTES))
}

fn new_core(world: &VideoWorld, data: &Path) -> DaemonCore {
    DaemonCore::with_store(QueueStore::in_dir(data))
        .unwrap()
        .with_tools(Arc::clone(&world.store))
}

impl Setup {
    /// The YouTube-like page, with 137 and 140 served by the stream server.
    fn page(&self) -> PageSpec {
        let mut spec = youtube_like(&self.server.url());
        for format in &mut spec.formats {
            if format.http_chunk_size.is_some() {
                format.http_chunk_size = Some(CHUNK);
            }
            let (path, size) = match format.format_id.as_str() {
                "137" => (VIDEO_PATH, VIDEO_BYTES),
                "140" => (AUDIO_PATH, AUDIO_BYTES),
                _ => continue,
            };
            format.url = self.server.url().join(path).unwrap().to_string();
            format.filesize = Some(size);
        }
        spec
    }

    fn answer(&self, spec: &PageSpec) {
        let response = ToolResponse {
            stdout: video_page_json(spec),
            ..ToolResponse::default()
        };
        let script = ToolScript {
            responses: vec![response],
        };
        write_script(&self.world.ytdlp.exe, &script);
    }

    fn add(&self, quality: Option<&str>) -> DownloadSummary {
        self.core
            .add_download(AddDownloadParams {
                url: self.server.url().join("/watch").unwrap().to_string(),
                destination: Some(self.folder.path().to_path_buf()),
                filename: None,
                connections: Some(2),
                max_bytes_per_sec: None,
                context: None,
                quality: quality.map(String::from),
                added_by: None,
                choice_id: None,
                page: None,
            })
            .unwrap()
    }

    /// Add the page with `quality` chosen at once.
    fn add_page(&self, quality: &str) -> DownloadSummary {
        self.answer(&self.page());
        self.add(Some(quality))
    }

    fn requests_to(&self, path: &str) -> Vec<RequestLog> {
        requests_to(&self.server.requests(), path)
    }

    fn file(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.folder.path().join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
    }
}

/// What the stand-in ffmpeg makes of the two streams: the video, then the audio.
fn joined_bytes() -> Vec<u8> {
    [pattern_bytes(VIDEO_BYTES), pattern_bytes(AUDIO_BYTES)].concat()
}

fn requests_to(requests: &[RequestLog], path: &str) -> Vec<RequestLog> {
    requests
        .iter()
        .filter(|r| r.path == path)
        .cloned()
        .collect()
}

/// Byte counts of the ranges asked for, the size probe left out.
fn range_sizes(requests: &[RequestLog]) -> Vec<u64> {
    requests
        .iter()
        .filter_map(|r| r.range.as_deref())
        .filter(|range| *range != PROBE_RANGE)
        .map(|range| {
            let (start, end) = range
                .strip_prefix("bytes=")
                .and_then(|bounds| bounds.split_once('-'))
                .expect("a bytes=a-b range");
            end.parse::<u64>().unwrap() - start.parse::<u64>().unwrap() + 1
        })
        .collect()
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

/// Tick the telemetry until the row's step passes `wanted`; returns that row.
async fn wait_for_step(
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

/// `video 1 of 2 · 37%`: a kind, the stream's place, and a whole percent.
fn is_stream_step(step: &str, kind: &str, index: usize, count: usize) -> bool {
    let Some(rest) = step.strip_prefix(&format!("{kind} {index} of {count} · ")) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix('%') else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

#[tokio::test]
async fn audio_only_downloads_one_stream_and_ends_with_the_final_name() {
    let s = setup().await;

    let added = s.add_page("audio");
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.filename, "A test video.m4a");
    assert_eq!(done.destination, s.folder.path().join("A test video.m4a"));
    assert_eq!(s.file("A test video.m4a"), pattern_bytes(AUDIO_BYTES));
    assert_eq!(names_in(s.folder.path()), ["A test video.m4a"]);
    assert!(s.requests_to(VIDEO_PATH).is_empty());
}

#[tokio::test]
async fn two_streams_download_one_after_the_other_in_capped_requests() {
    let s = setup().await;
    s.answer(&s.page());
    let added = s.add(None);
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;

    let chosen = s
        .core
        .choose_quality(ChooseQualityParams {
            id: added.id.clone(),
            choice_id: "1080-mp4".to_string(),
            destination: None,
            filename: None,
        })
        .unwrap();
    assert_eq!(chosen.status, DownloadStatus::Queued);
    let ended = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(ended.error_message, None);
    assert_eq!(s.file("A test video.mp4"), joined_bytes());
    assert_eq!(names_in(s.folder.path()), ["A test video.mp4"]);
    let streams: Vec<RequestLog> = s
        .server
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/stream/"))
        .collect();
    let sizes = range_sizes(&streams);
    assert!(sizes.len() >= 5, "{sizes:?}");
    assert!(sizes.iter().all(|&n| n <= CHUNK), "{sizes:?}");
    let last_video = streams.iter().rposition(|r| r.path == VIDEO_PATH).unwrap();
    let first_audio = streams.iter().position(|r| r.path == AUDIO_PATH).unwrap();
    assert!(last_video < first_audio, "the streams overlapped");
}

#[tokio::test]
async fn the_row_counts_both_streams() {
    let s = setup_with(
        video_route().throttled(400_000),
        audio_route().throttled(150_000),
    )
    .await;

    let added = s.add_page("1080p");
    let running = wait_for_step(&s.core, &added.id, |row| {
        row.step
            .as_deref()
            .is_some_and(|step| is_stream_step(step, "video", 1, 2))
    })
    .await;

    assert_eq!(running.status, DownloadStatus::Active);
    assert_eq!(running.total_bytes, Some(VIDEO_BYTES + AUDIO_BYTES));
    assert_eq!(running.filename, "A test video.mp4");
    assert_eq!(
        running.destination,
        s.folder.path().join("A test video.mp4")
    );
    assert!(running.downloaded_bytes < VIDEO_BYTES);
    let audio = wait_for_step(&s.core, &added.id, |row| {
        row.step
            .as_deref()
            .is_some_and(|step| is_stream_step(step, "audio", 2, 2))
    })
    .await;
    // The audio counts on from the finished video.
    assert!(audio.downloaded_bytes >= VIDEO_BYTES, "{audio:?}");
    let ended = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;
    assert_eq!(ended.total_bytes, Some(VIDEO_BYTES + AUDIO_BYTES));
}

#[tokio::test]
async fn only_allowed_headers_reach_the_stream_server() {
    let s = setup().await;
    let mut spec = s.page();
    for format in &mut spec.formats {
        format.http_headers.extend(
            [("Cookie", "x"), ("Authorization", "y")]
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
    }
    s.answer(&spec);

    let added = s.add(Some("audio"));
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    let requests = s.requests_to(AUDIO_PATH);
    assert!(!requests.is_empty());
    for request in requests {
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(header("sec-fetch-mode").as_deref(), Some("navigate"));
        assert_eq!(header("authorization"), None);
        assert!(
            header("cookie").is_none_or(|cookie| !cookie.contains('x')),
            "{request:?}"
        );
    }
}

#[tokio::test]
async fn a_restart_mid_stream_continues_the_same_stream_file() {
    let s = setup_with(video_route().throttled(300_000), audio_route()).await;
    let added = s.add_page("1080p");
    wait_for_step(&s.core, &added.id, |row| {
        row.downloaded_bytes >= 600_000
            && row
                .step
                .as_deref()
                .is_some_and(|step| is_stream_step(step, "video", 1, 2))
    })
    .await;

    s.core.shutdown();
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);
    let before = s.server.requests().len();
    s.server.set_route(VIDEO_PATH, video_route());
    let again = new_core(&s.world, s.data.path());
    again.schedule();
    let ended = wait_for(&again, &added.id, DownloadStatus::Completed).await;

    assert_eq!(ended.error_message, None);
    let names = names_in(s.folder.path());
    assert_eq!(names, ["A test video.mp4"]);
    assert_eq!(s.file("A test video.mp4"), joined_bytes());
    let after = requests_to(&s.server.requests()[before..], VIDEO_PATH);
    let fetched: u64 = range_sizes(&after).iter().sum();
    assert!(fetched < VIDEO_BYTES, "fetched {fetched} after the restart");
}

#[tokio::test]
async fn cancelling_a_video_with_delete_removes_its_stream_files() {
    let s = setup_with(video_route(), audio_route().throttled(60_000)).await;
    std::fs::write(s.folder.path().join("keep.txt"), "mine").unwrap();
    let added = s.add_page("1080p");
    wait_for_step(&s.core, &added.id, |row| {
        row.step
            .as_deref()
            .is_some_and(|step| is_stream_step(step, "audio", 2, 2))
    })
    .await;

    s.core
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: true,
        })
        .unwrap();
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);

    assert_eq!(
        s.core.get_status(&added.id).unwrap().status,
        DownloadStatus::Cancelled
    );
    assert_eq!(names_in(s.folder.path()), ["keep.txt"]);
}

#[tokio::test]
async fn a_title_with_forbidden_characters_gives_safe_stream_names() {
    let s = setup().await;
    let mut spec = s.page();
    spec.title = "A: b/c?".to_string();
    s.answer(&spec);

    // A failed join keeps the stream files, which are the ones to look at.
    let fails = ToolResponse {
        exit_code: 1,
        ..ToolResponse::default()
    };
    write_script(
        &s.world.ffmpeg.exe,
        &ToolScript {
            responses: vec![fails],
        },
    );

    let added = s.add(Some("1080p"));
    wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    let names = names_in(s.folder.path());
    assert_eq!(names.len(), 2, "{names:?}");
    for name in &names {
        assert!(s.folder.path().join(name).is_file(), "{name}");
        assert!(!name.contains([':', '/', '?', '\\']), "{name}");
    }
    assert!(names.iter().any(|n| n.ends_with(".f137.mp4")), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with(".f140.m4a")), "{names:?}");
}

#[tokio::test]
async fn streams_without_a_size_still_download() {
    let s = setup().await;
    let mut spec = s.page();
    for format in &mut spec.formats {
        format.filesize = None;
        format.filesize_approx = None;
    }
    s.answer(&spec);

    let added = s.add(Some("audio"));
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.error_message, None);
    assert_eq!(s.file("A test video.m4a"), pattern_bytes(AUDIO_BYTES));
}

#[tokio::test]
async fn a_video_too_big_for_the_disk_fails_before_any_stream() {
    let s = setup().await;
    let mut spec = s.page();
    for format in &mut spec.formats {
        format.filesize = Some(1 << 40);
    }
    s.answer(&spec);

    let added = s.add(Some("1080p"));
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    let message = failed.error_message.unwrap();
    assert!(message.starts_with("Needs 4.0 TiB free on "), "{message}");
    assert!(
        message.contains(
            " (the video is joined from two parts, so it briefly needs about twice its size); "
        ),
        "{message}"
    );
    assert!(message.ends_with(" free."), "{message}");
    assert!(s.requests_to(VIDEO_PATH).is_empty());
    assert!(names_in(s.folder.path()).is_empty());
}

fn stream(format_id: &str, kind: StreamKind, size: Option<u64>) -> StreamPlan {
    StreamPlan {
        format_id: format_id.to_string(),
        kind,
        ext: "mp4".to_string(),
        url: "https://video.test/stream".to_string(),
        size,
        headers: Vec::new(),
        request_bytes: None,
        file: None,
        source: StreamSource::Direct,
    }
}

fn plan_of(streams: Vec<StreamPlan>) -> VideoPlan {
    VideoPlan {
        page_url: "https://video.test/watch".to_string(),
        title: "A video".to_string(),
        duration_secs: None,
        choice: None,
        choices: Vec::new(),
        offered: Vec::new(),
        default: 0,
        streams,
        step: VideoStep::Stream(0),
        refreshes: Vec::new(),
        site: String::new(),
        origin: rustdm::video::PlanOrigin::Page,
    }
}

#[test]
fn the_disk_check_asks_for_twice_the_size_when_joining() {
    let video = || stream("137", StreamKind::Video, Some(VIDEO_BYTES));
    let audio = |size| stream("140", StreamKind::Audio, size);

    assert_eq!(
        video_space_needed(&plan_of(vec![video(), audio(Some(AUDIO_BYTES))])),
        Some(2 * (VIDEO_BYTES + AUDIO_BYTES))
    );
    assert_eq!(
        video_space_needed(&plan_of(vec![audio(Some(AUDIO_BYTES))])),
        Some(AUDIO_BYTES)
    );
    assert_eq!(
        video_space_needed(&plan_of(vec![video(), audio(None)])),
        None
    );
}
