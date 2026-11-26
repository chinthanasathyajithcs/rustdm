//! Video pages whose streams come in pieces (HLS, DASH) download through
//! pieces mode and end as one file (spec 1c, sections 5, 7, 9).

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, DownloadStatus, DownloadSummary,
};
use rustdm::testing::video::{
    browser_headers, recorded_args, video_page_json, write_script, FormatSpec, PageSpec,
    ToolResponse, ToolScript, VideoWorld,
};
use rustdm::testing::{Route, TestServer};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const VIDEO_PIECES: usize = 8;
const AUDIO_PIECES: usize = 4;
const PIECE_BYTES: usize = 20_000;
/// About half a second a piece, so a test can act part-way through.
const SLOW: u64 = 40_000;
const TITLE_FILE: &str = "A test video.mp4";
const LIVE: &str = "This is a live stream; rustdm downloads finished videos only.";
const PROTECTED: &str = "This video is copy-protected; rustdm can't download it.";

fn piece_body(stream: char, i: usize) -> Vec<u8> {
    let seed = stream as usize * 31 + i * 7;
    (0..PIECE_BYTES).map(|j| (seed + j) as u8).collect()
}

fn stream_bytes(stream: char, n: usize) -> Vec<u8> {
    (0..n).flat_map(|i| piece_body(stream, i)).collect()
}

fn joined() -> Vec<u8> {
    [
        stream_bytes('v', VIDEO_PIECES),
        stream_bytes('a', AUDIO_PIECES),
    ]
    .concat()
}

/// A media playlist of `n` pieces at `<dir>/<i>`.
fn media_playlist(dir: &str, n: usize, ended: bool, key: Option<&str>) -> String {
    let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    if let Some(key) = key {
        text.push_str(key);
        text.push('\n');
    }
    for i in 0..n {
        text.push_str(&format!("#EXTINF:4.0,\n{dir}/{i}\n"));
    }
    if ended {
        text.push_str("#EXT-X-ENDLIST\n");
    }
    text
}

fn playlist_route(text: String) -> Route {
    Route::ok(text).with_header("Content-Type", "application/vnd.apple.mpegurl")
}

struct Setup {
    world: VideoWorld,
    server: TestServer,
    data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

async fn setup(slow: bool) -> Setup {
    let world = VideoWorld::new().await;
    let page = Route::ok("<html>a page</html>").with_header("Content-Type", "text/html");
    let mut routes = vec![
        ("/watch".to_string(), page.clone()),
        ("/watch2".to_string(), page),
        (
            "/hls/v.m3u8".to_string(),
            playlist_route(media_playlist("v", VIDEO_PIECES, true, None)),
        ),
        (
            "/hls/a.m3u8".to_string(),
            playlist_route(media_playlist("a", AUDIO_PIECES, true, None)),
        ),
        // The same video at other links, as a refresh gives it.
        (
            "/hls2/v.m3u8".to_string(),
            playlist_route(media_playlist("v", VIDEO_PIECES, true, None)),
        ),
        // Another video: one piece more.
        (
            "/hls3/v.m3u8".to_string(),
            playlist_route(media_playlist("v", VIDEO_PIECES + 1, true, None)),
        ),
    ];
    for (stream, n) in [('v', VIDEO_PIECES), ('a', AUDIO_PIECES)] {
        for i in 0..n {
            let route = Route::ok(piece_body(stream, i));
            let route = if slow && stream == 'v' {
                route.throttled(SLOW)
            } else {
                route
            };
            routes.push((format!("/hls/{stream}/{i}"), route.clone()));
            routes.push((format!("/hls2/{stream}/{i}"), route.clone()));
            routes.push((format!("/hls3/{stream}/{i}"), route.clone()));
            routes.push((format!("/dash/{stream}/{i}"), route));
        }
    }
    let server = TestServer::start_routes(routes).await;
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

fn new_core(world: &VideoWorld, data: &Path) -> DaemonCore {
    DaemonCore::with_store(QueueStore::in_dir(data))
        .unwrap()
        .with_tools(Arc::clone(&world.store))
}

impl Setup {
    fn link(&self, path: &str) -> String {
        self.server.url().join(path).unwrap().to_string()
    }

    fn format(&self, id: &str, protocol: &str, video: bool, url: String) -> FormatSpec {
        FormatSpec {
            format_id: id.to_string(),
            protocol: protocol.to_string(),
            ext: if video { "mp4" } else { "m4a" }.to_string(),
            vcodec: if video { "avc1.640028" } else { "none" }.to_string(),
            acodec: if video { "none" } else { "mp4a.40.2" }.to_string(),
            height: video.then_some(1080),
            tbr: Some(if video { 800.0 } else { 128.0 }),
            filesize: None,
            filesize_approx: None,
            language_preference: None,
            format_note: None,
            has_drm: false,
            http_chunk_size: None,
            http_headers: browser_headers(),
            url,
            fragment_base_url: None,
            fragments: Vec::new(),
        }
    }

    fn hls_formats(&self) -> Vec<FormatSpec> {
        vec![
            self.format("hls-1080", "m3u8_native", true, self.link("/hls/v.m3u8")),
            self.format("hls-audio", "m3u8_native", false, self.link("/hls/a.m3u8")),
        ]
    }

    fn dash_formats(&self) -> Vec<FormatSpec> {
        let dash = |id: &str, video: bool, stream: char, n: usize| FormatSpec {
            fragment_base_url: Some(self.link("/dash/")),
            fragments: (0..n)
                .map(|i| (format!("{stream}/{i}"), (i > 0).then_some(4.0)))
                .collect(),
            ..self.format(
                id,
                "http_dash_segments",
                video,
                self.link("/dash/manifest.mpd"),
            )
        };
        vec![
            dash("dash-v", true, 'v', VIDEO_PIECES),
            dash("dash-a", false, 'a', AUDIO_PIECES),
        ]
    }

    fn answer(&self, formats: Vec<FormatSpec>) {
        self.answers(vec![formats]);
    }

    /// yt-dlp answers with these pages in turn, the last from then on.
    fn answers(&self, pages: Vec<Vec<FormatSpec>>) {
        let responses = pages
            .into_iter()
            .map(|formats| {
                let spec = PageSpec {
                    title: "A test video".to_string(),
                    extractor_key: "Generic".to_string(),
                    duration: 32,
                    is_live: false,
                    playlist_count: None,
                    formats,
                };
                ToolResponse {
                    stdout: video_page_json(&spec),
                    ..ToolResponse::default()
                }
            })
            .collect();
        write_script(&self.world.ytdlp.exe, &ToolScript { responses });
    }

    /// The HLS pair with the video's playlist at `path`.
    fn hls_formats_at(&self, path: &str) -> Vec<FormatSpec> {
        let mut formats = self.hls_formats();
        formats[0].url = self.link(path);
        formats
    }

    fn asked(&self, path: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|r| r.path == path)
            .count()
    }

    fn add(&self, quality: &str) -> DownloadSummary {
        self.add_at("/watch", quality)
    }

    fn add_at(&self, page: &str, quality: &str) -> DownloadSummary {
        self.core
            .add_download(AddDownloadParams {
                url: self.link(page),
                destination: Some(self.folder.path().to_path_buf()),
                filename: None,
                connections: Some(2),
                max_bytes_per_sec: None,
                context: None,
                quality: Some(quality.to_string()),
                added_by: None,
            })
            .unwrap()
    }

    fn file(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.folder.path().join(name)).unwrap()
    }

    /// Requests for the video's pieces under `/<kind>/v/`.
    fn video_piece_requests(&self, kind: &str) -> usize {
        let prefix = format!("/{kind}/v/");
        self.server
            .requests()
            .iter()
            .filter(|r| r.path.starts_with(&prefix))
            .count()
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

/// The pieces finished, from a row reading `video 1 of 2 · 45% · piece 3 of 8`.
fn pieces_done(row: &DownloadSummary) -> Option<usize> {
    let step = row.step.as_deref()?;
    let (_, tail) = step.split_once("· piece ")?;
    tail.split(' ').next()?.parse().ok()
}

#[tokio::test]
async fn an_hls_video_and_audio_download_and_join() {
    let s = setup(false).await;
    s.answer(s.hls_formats());

    let added = s.add("1080p");
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.error_message, None);
    assert_eq!(names_in(s.folder.path()), [TITLE_FILE]);
    assert_eq!(s.file(TITLE_FILE), joined());
}

#[tokio::test]
async fn a_dash_pair_downloads_and_joins() {
    let s = setup(false).await;
    s.answer(s.dash_formats());

    let added = s.add("1080p");
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), [TITLE_FILE]);
    assert_eq!(s.file(TITLE_FILE), joined());
}

#[tokio::test]
async fn the_row_counts_pieces() {
    let s = setup(true).await;
    s.answer(s.hls_formats());

    let added = s.add("1080p");
    let row = wait_until(&s.core, &added.id, |r| {
        r.step.as_deref().is_some_and(|step| {
            step.starts_with("video 1 of 2 · ")
                && step.contains("· piece ")
                && step.ends_with(" of 8")
        })
    })
    .await;

    assert!(
        row.video.as_ref().is_some_and(|v| v.size_is_estimate),
        "{row:?}"
    );
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;
}

#[tokio::test]
async fn pause_and_resume_ask_only_unfinished_pieces_again() {
    let s = setup(true).await;
    s.answer(s.hls_formats());
    let added = s.add("1080p");
    wait_until(&s.core, &added.id, |r| {
        pieces_done(r).is_some_and(|n| n >= 3)
    })
    .await;

    s.core.pause_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Paused).await;
    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.file(TITLE_FILE), joined());
    // Only pieces cut short by the pause (at most one a connection) come again.
    let asked = s.video_piece_requests("hls");
    assert!(
        asked <= VIDEO_PIECES + 2,
        "{asked} requests for {VIDEO_PIECES} pieces"
    );
}

#[tokio::test]
async fn a_resumed_row_keeps_its_kept_bytes_while_the_playlist_is_read() {
    let s = setup(true).await;
    s.answer(s.hls_formats());
    let added = s.add("1080p");
    wait_until(&s.core, &added.id, |r| {
        pieces_done(r).is_some_and(|n| n >= 3)
    })
    .await;
    s.core.pause_download(&added.id).unwrap();
    let paused = wait_for(&s.core, &added.id, DownloadStatus::Paused).await;
    assert!(paused.downloaded_bytes > 0);
    // The playlist is slow now, so the resumed run waits on it a while.
    let slow_playlist = playlist_route(media_playlist("v", VIDEO_PIECES, true, None)).throttled(20);
    s.server.set_route("/hls/v.m3u8", slow_playlist);

    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Active).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.core.tick_telemetry();
    let reading = s.core.get_status(&added.id).unwrap();

    assert_eq!(reading.downloaded_bytes, paused.downloaded_bytes);
}

#[tokio::test]
async fn the_details_pane_sees_each_running_piece() {
    let s = setup(true).await;
    s.answer(s.hls_formats());
    let mut ticks = s.core.telemetry_tx.subscribe();
    let added = s.add("1080p");
    wait_until(&s.core, &added.id, |r| {
        pieces_done(r).is_some_and(|n| n >= 1)
    })
    .await;
    s.core.tick_telemetry();

    let mut map = None;
    while let Ok(tick) = ticks.try_recv() {
        if tick.id == added.id && tick.pieces.as_ref().is_some_and(|m| !m.running.is_empty()) {
            map = Some((tick.active_connections, tick.pieces.unwrap()));
        }
    }
    let (connections, map) = map.expect("a tick with running pieces");

    assert_eq!(map.total, VIDEO_PIECES as u64);
    assert_eq!(connections, map.running.len());
    assert!(map.written + map.waiting.len() as u64 >= 1);
    for lane in &map.running {
        assert!(lane.index >= map.written, "{lane:?}");
        assert!(!map.waiting.contains(&lane.index), "{lane:?}");
    }
}

#[tokio::test]
async fn a_crash_keeps_finished_pieces() {
    let s = setup(true).await;
    s.answer(s.hls_formats());
    let added = s.add("1080p");
    wait_until(&s.core, &added.id, |r| {
        pieces_done(r).is_some_and(|n| n >= 3)
    })
    .await;

    s.core.shutdown();
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);
    let again = new_core(&s.world, s.data.path());
    again.schedule();
    wait_for(&again, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), [TITLE_FILE]);
    assert_eq!(s.file(TITLE_FILE), joined());
    let asked = s.video_piece_requests("hls");
    assert!(
        asked <= VIDEO_PIECES + 2,
        "{asked} requests for {VIDEO_PIECES} pieces"
    );
}

#[tokio::test]
async fn cancel_with_delete_removes_pieces_files() {
    let s = setup(true).await;
    std::fs::write(s.folder.path().join("keep.txt"), "mine").unwrap();
    s.answer(s.hls_formats());
    let added = s.add("1080p");
    wait_until(&s.core, &added.id, |r| {
        pieces_done(r).is_some_and(|n| n >= 2)
    })
    .await;

    s.core
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: true,
        })
        .unwrap();
    assert!(s.core.wait_until_settled(Duration::from_secs(10)).await);

    assert_eq!(names_in(s.folder.path()), ["keep.txt"]);
}

#[tokio::test]
async fn two_pieces_downloads_of_one_title_keep_apart() {
    let s = setup(true).await;
    s.answer(s.hls_formats());

    let first = s.add("1080p");
    let second = s.add_at("/watch2", "1080p");
    wait_for(&s.core, &first.id, DownloadStatus::Completed).await;
    wait_for(&s.core, &second.id, DownloadStatus::Completed).await;

    assert_eq!(
        names_in(s.folder.path()),
        ["A test video (1).mp4", TITLE_FILE]
    );
    assert_eq!(s.file(TITLE_FILE), joined());
    assert_eq!(s.file("A test video (1).mp4"), joined());
}

#[tokio::test]
async fn a_live_playlist_stops_with_the_live_message() {
    let s = setup(false).await;
    s.server.set_route(
        "/hls/v.m3u8",
        playlist_route(media_playlist("v", VIDEO_PIECES, false, None)),
    );
    s.answer(s.hls_formats());

    let added = s.add("1080p");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(LIVE));
}

#[tokio::test]
async fn a_protected_playlist_stops_with_the_protected_message() {
    let s = setup(false).await;
    let key = "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\"";
    s.server.set_route(
        "/hls/v.m3u8",
        playlist_route(media_playlist("v", VIDEO_PIECES, true, Some(key))),
    );
    s.answer(s.hls_formats());

    let added = s.add("1080p");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(PROTECTED));
}

#[tokio::test]
async fn a_single_hls_stream_is_remuxed() {
    let s = setup(false).await;
    let whole = FormatSpec {
        acodec: "mp4a.40.2".to_string(),
        height: Some(720),
        ..s.format("hls-720", "m3u8_native", true, s.link("/hls/v.m3u8"))
    };
    s.answer(vec![whole]);

    let added = s.add("720p");
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), [TITLE_FILE]);
    assert_eq!(s.file(TITLE_FILE), stream_bytes('v', VIDEO_PIECES));
    let calls = recorded_args(&s.world.ffmpeg.exe);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let args = &calls[0];
    let maps: Vec<&String> = args
        .windows(2)
        .filter(|pair| pair[0] == "-map")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(maps, ["0:v?", "0:a?"]);
    assert!(
        args.last().unwrap().ends_with("A test video.mp4.joining"),
        "{args:?}"
    );
}

#[tokio::test]
async fn an_expired_piece_refreshes_through_yt_dlp() {
    let s = setup(false).await;
    s.server.set_route("/hls/v/5", Route::status(403));
    s.answers(vec![s.hls_formats(), s.hls_formats_at("/hls2/v.m3u8")]);

    let added = s.add("1080p");
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.file(TITLE_FILE), joined());
    assert_eq!(
        recorded_args(&s.world.ytdlp.exe).len(),
        2,
        "one lookup, one refresh"
    );
    for i in 0..2 {
        assert_eq!(s.asked(&format!("/hls/v/{i}")), 1, "piece {i}");
        assert_eq!(s.asked(&format!("/hls2/v/{i}")), 0, "piece {i} kept");
    }
    assert!(s.asked("/hls2/v/5") > 0);
}

#[tokio::test]
async fn a_refresh_with_other_pieces_is_a_changed_video() {
    let s = setup(false).await;
    s.server.set_route("/hls/v/5", Route::status(403));
    s.answers(vec![s.hls_formats(), s.hls_formats_at("/hls3/v.m3u8")]);

    let added = s.add("1080p");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some("This video changed on the site; download it again to get the new version.")
    );
}

#[tokio::test]
async fn a_paused_download_keeps_its_stream_name_from_another_of_the_same_title() {
    let s = setup(true).await;
    s.answer(s.hls_formats());
    // A's playlist is slow, so A is paused before its pieces start.
    let slow_playlist = playlist_route(media_playlist("v", VIDEO_PIECES, true, None)).throttled(20);
    s.server.set_route("/hls/v.m3u8", slow_playlist);
    let a = s.add("1080p");
    wait_until(&s.core, &a.id, |r| {
        r.step
            .as_deref()
            .is_some_and(|step| step.starts_with("video 1 of 2"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.core.pause_download(&a.id).unwrap();
    wait_for(&s.core, &a.id, DownloadStatus::Paused).await;
    s.server.set_route(
        "/hls/v.m3u8",
        playlist_route(media_playlist("v", VIDEO_PIECES, true, None)),
    );

    let b = s.add_at("/watch2", "1080p");
    wait_until(&s.core, &b.id, |r| pieces_done(r).is_some_and(|n| n >= 1)).await;
    s.core.resume_download(&a.id).unwrap();
    wait_for(&s.core, &a.id, DownloadStatus::Completed).await;
    wait_for(&s.core, &b.id, DownloadStatus::Completed).await;

    assert_eq!(
        names_in(s.folder.path()),
        ["A test video (1).mp4", TITLE_FILE]
    );
    assert_eq!(s.file(TITLE_FILE), joined());
    assert_eq!(s.file("A test video (1).mp4"), joined());
}

#[tokio::test]
async fn a_dash_refresh_with_other_durations_is_a_changed_video() {
    let s = setup(false).await;
    s.server.set_route("/dash/v/5", Route::status(403));
    let mut longer = s.dash_formats();
    for format in &mut longer {
        format.fragment_base_url = Some(s.link("/hls2/"));
        for fragment in &mut format.fragments {
            fragment.1 = fragment.1.map(|_| 5.0);
        }
    }
    s.answers(vec![s.dash_formats(), longer]);

    let added = s.add("1080p");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some("This video changed on the site; download it again to get the new version.")
    );
}
