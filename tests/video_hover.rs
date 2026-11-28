//! Looking a video up for the browser before the click (spec 1f): the answer
//! is kept, nothing is added, and a download can start from it.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::engine::build_client;
use rustdm::ipc::protocol::{LookUpParams, CODE_LOOKUP_FAILED};
use rustdm::testing::fake_releases::test_sources;
use rustdm::testing::video::{
    recorded_args, video_page_json, write_script, youtube_like, PageSpec, ToolResponse, ToolScript,
    VideoWorld,
};
use rustdm::testing::{pattern_bytes, Route, TestServer};
use rustdm::tools::ToolStore;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MEDIA: [&str; 3] = ["/media/136", "/media/137", "/media/140"];
const MASTER: &str = "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=1280x720
720/index.m3u8
";

struct Setup {
    world: VideoWorld,
    pages: TestServer,
    _data: tempfile::TempDir,
    core: DaemonCore,
}

fn html_page() -> Route {
    Route::ok("<html><body>a page</body></html>").with_header("Content-Type", "text/html")
}

async fn setup() -> Setup {
    let world = VideoWorld::new().await;
    let mut routes: Vec<(String, Route)> = ["/watch", "/watch1", "/watch2", "/watch.html"]
        .map(|path| (path.to_string(), html_page()))
        .to_vec();
    routes.extend(MEDIA.map(|path| (path.to_string(), Route::ok(pattern_bytes(4096)))));
    routes.push((
        "/m/master.m3u8".to_string(),
        Route::ok(MASTER).with_header("Content-Type", "application/vnd.apple.mpegurl"),
    ));
    let pages = TestServer::start_routes(routes).await;
    let data = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(Arc::clone(&world.store));
    Setup {
        world,
        pages,
        _data: data,
        core,
    }
}

fn page_answer(spec: &PageSpec) -> ToolResponse {
    ToolResponse {
        stdout: video_page_json(spec),
        ..ToolResponse::default()
    }
}

impl Setup {
    fn answer(&self, responses: Vec<ToolResponse>) {
        write_script(&self.world.ytdlp.exe, &ToolScript { responses });
    }

    fn url(&self, path: &str) -> String {
        self.pages.url().join(path).unwrap().to_string()
    }

    fn page(&self) -> PageSpec {
        youtube_like(&self.pages.url())
    }

    fn lookups(&self) -> usize {
        recorded_args(&self.world.ytdlp.exe).len()
    }

    fn ask(&self, path: &str) -> LookUpParams {
        LookUpParams {
            url: self.url(path),
            context: None,
        }
    }
}

#[tokio::test]
async fn a_look_up_answers_with_the_choices_and_adds_nothing() {
    let s = setup().await;
    s.answer(vec![page_answer(&s.page())]);
    let found = s.core.look_up_for_hover(s.ask("/watch")).await.unwrap();
    assert!(found.choices.len() > 1, "{:?}", found.choices);
    assert_eq!(found.choices[found.default].id, "1080-mp4");
    assert!(s.core.list_downloads().is_empty());
}

#[tokio::test]
async fn a_second_look_up_of_the_same_page_runs_no_tool() {
    let s = setup().await;
    s.answer(vec![page_answer(&s.page()), page_answer(&s.page())]);
    s.core.look_up_for_hover(s.ask("/watch")).await.unwrap();
    s.core.look_up_for_hover(s.ask("/watch")).await.unwrap();
    assert_eq!(s.lookups(), 1);
}

#[tokio::test]
async fn a_new_look_up_stops_the_one_running() {
    let s = setup().await;
    let slow = ToolResponse {
        sleep_ms: 20_000,
        ..page_answer(&s.page())
    };
    s.answer(vec![slow, page_answer(&s.page())]);
    let core = s.core.clone();
    let first_ask = s.ask("/watch1");
    let first = tokio::spawn(async move { core.look_up_for_hover(first_ask).await });
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.lookups() == 0 {
        assert!(Instant::now() < deadline, "the first lookup never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let second = s.core.look_up_for_hover(s.ask("/watch2")).await;
    assert!(second.is_ok(), "{second:?}");
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first lookup was stopped")
        .unwrap();
    let error = first.unwrap_err();
    assert_eq!(error.code, CODE_LOOKUP_FAILED);
    assert_eq!(error.message, "cancelled");
}

#[tokio::test]
async fn a_look_up_never_fetches_tools() {
    let releases = TestServer::start_routes(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = ToolStore::with_sources(
        dir.path().join("tools"),
        build_client(),
        test_sources(&releases),
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(Arc::new(store));
    let ask = LookUpParams {
        url: "https://video.test/watch".to_string(),
        context: None,
    };
    let error = core.look_up_for_hover(ask).await.unwrap_err();
    assert_eq!(error.code, CODE_LOOKUP_FAILED);
    assert!(releases.requests().is_empty(), "{:?}", releases.requests());
}

#[tokio::test]
async fn a_caught_playlist_is_read_without_yt_dlp() {
    let s = setup().await;
    let found = s
        .core
        .look_up_for_hover(s.ask("/m/master.m3u8"))
        .await
        .unwrap();
    assert_eq!(found.choices.len(), 2, "{:?}", found.choices);
    assert_eq!(s.lookups(), 0);
}
