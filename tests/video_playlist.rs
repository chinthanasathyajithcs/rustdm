//! A direct `.m3u8` link downloads without yt-dlp; the quality list appears
//! only when the playlist offers several qualities (spec 1c, section 6).

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, ChooseQualityParams, DownloadStatus, DownloadSummary,
};
use rustdm::testing::video::{recorded_args, VideoWorld};
use rustdm::testing::{Route, TestServer};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PIECES: usize = 5;
const PIECE_BYTES: usize = 10_000;
const UNREADABLE: &str = "This video's playlist couldn't be read.";
const LIVE: &str = "This is a live stream; rustdm downloads finished videos only.";

const MASTER: &str = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"aud/index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"aud\"
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"aud\"
720/index.m3u8
";

fn piece_body(stream: &str, i: usize) -> Vec<u8> {
    let seed = stream.len() * 13 + stream.as_bytes()[0] as usize + i * 7;
    (0..PIECE_BYTES).map(|j| (seed + j) as u8).collect()
}

fn stream_bytes(stream: &str) -> Vec<u8> {
    (0..PIECES).flat_map(|i| piece_body(stream, i)).collect()
}

/// A media playlist of pieces `p0`… next to it.
fn media(ended: bool) -> String {
    let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    for i in 0..PIECES {
        text.push_str(&format!("#EXTINF:4.0,\np{i}\n"));
    }
    if ended {
        text.push_str("#EXT-X-ENDLIST\n");
    }
    text
}

fn hls(text: impl Into<Vec<u8>>) -> Route {
    Route::ok(text).with_header("Content-Type", "application/vnd.apple.mpegurl")
}

struct Setup {
    world: VideoWorld,
    server: TestServer,
    _data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

async fn setup() -> Setup {
    let world = VideoWorld::new().await;
    let mut routes = vec![
        ("/m/master.m3u8".to_string(), hls(MASTER)),
        ("/m/single.m3u8".to_string(), hls(media(true))),
        ("/v/x/index.m3u8".to_string(), hls(media(true))),
        (
            "/go".to_string(),
            Route::status(302).with_header("Location", "/v/x/index.m3u8?tok=1"),
        ),
        (
            "/bad.m3u8".to_string(),
            Route::ok("not a playlist").with_header("Content-Type", "application/octet-stream"),
        ),
        ("/live.m3u8".to_string(), hls(media(false))),
    ];
    for dir in ["1080", "720", "aud"] {
        routes.push((format!("/m/{dir}/index.m3u8"), hls(media(true))));
    }
    for (dir, stream) in [
        ("/m/1080", "1080"),
        ("/m/720", "720"),
        ("/m/aud", "aud"),
        ("/m", "single"),
        ("/v/x", "single"),
    ] {
        for i in 0..PIECES {
            routes.push((format!("{dir}/p{i}"), Route::ok(piece_body(stream, i))));
        }
    }
    let server = TestServer::start_routes(routes).await;
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(Arc::clone(&world.store));
    Setup {
        world,
        server,
        _data: data,
        folder,
        core,
    }
}

impl Setup {
    fn add(&self, path: &str, quality: Option<&str>, filename: Option<&str>) -> DownloadSummary {
        self.core
            .add_download(AddDownloadParams {
                url: self.server.url().join(path).unwrap().to_string(),
                destination: Some(self.folder.path().to_path_buf()),
                filename: filename.map(String::from),
                connections: Some(2),
                max_bytes_per_sec: None,
                context: None,
                quality: quality.map(String::from),
            })
            .unwrap()
    }

    fn file(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.folder.path().join(name)).unwrap()
    }

    fn asked_under(&self, prefix: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|r| r.path.starts_with(prefix))
            .count()
    }

    fn no_lookup(&self) {
        assert!(
            recorded_args(&self.world.ytdlp.exe).is_empty(),
            "yt-dlp was not asked"
        );
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
        core.tick_telemetry();
        let summary = core.get_status(id).unwrap();
        if summary.status == want {
            return summary;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never got to {want:?}: {:?} {:?} {:?}",
            summary.status,
            summary.step,
            summary.error_message
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn one_quality_starts_at_once() {
    let s = setup().await;

    let added = s.add("/m/single.m3u8", None, None);
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), ["single.mp4"]);
    assert_eq!(s.file("single.mp4"), stream_bytes("single"));
    s.no_lookup();
}

#[tokio::test]
async fn several_qualities_wait_for_a_choice() {
    let s = setup().await;

    let added = s.add("/m/master.m3u8", None, None);
    let waiting = wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;
    let video = waiting.video.expect("a video summary");
    let labels: Vec<&str> = video.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["1080p", "720p", "Audio only"]);

    s.core
        .choose_quality(ChooseQualityParams {
            id: added.id.clone(),
            choice_id: "720-mp4".to_string(),
            destination: None,
            filename: None,
        })
        .unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), ["m.mp4"]);
    assert_eq!(
        s.file("m.mp4"),
        [stream_bytes("720"), stream_bytes("aud")].concat()
    );
    assert_eq!(
        s.asked_under("/m/1080/p"),
        0,
        "the 1080p pieces were not fetched"
    );
    s.no_lookup();
}

#[tokio::test]
async fn a_quality_given_with_the_link_is_used() {
    let s = setup().await;

    let added = s.add("/m/master.m3u8", Some("720p"), None);
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.asked_under("/m/720/p"), PIECES);
    assert_eq!(s.asked_under("/m/1080/p"), 0);
}

#[tokio::test]
async fn a_redirected_playlist_resolves_its_pieces() {
    let s = setup().await;

    let added = s.add("/go", None, None);
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.asked_under("/v/x/p"), PIECES);
    assert_eq!(s.file("x.mp4"), stream_bytes("single"));
}

#[tokio::test]
async fn a_page_like_answer_is_unreadable() {
    let s = setup().await;

    let added = s.add("/bad.m3u8", None, None);
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(UNREADABLE));
}

#[tokio::test]
async fn a_live_playlist_link_is_refused() {
    let s = setup().await;

    let added = s.add("/live.m3u8", None, None);
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(LIVE));
}

#[tokio::test]
async fn a_requested_name_wins() {
    let s = setup().await;

    let added = s.add("/m/single.m3u8", None, Some("Mine"));
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(names_in(s.folder.path()), ["Mine.mp4"]);
}
