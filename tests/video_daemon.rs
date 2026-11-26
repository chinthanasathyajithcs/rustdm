//! The daemon looks web pages up with yt-dlp (the stand-in program) and waits
//! for a quality to be chosen.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::task::MAX_AUTO_RETRIES;
use rustdm::daemon::DaemonCore;
use rustdm::engine::build_client;
use rustdm::error::web_page_with_cookies_message;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, ChooseQualityParams, DownloadStatus, DownloadSummary,
    RpcId, RpcRequest, CODE_INVALID_PARAMS, CODE_INVALID_PATH,
};
use rustdm::request_context::{ContextCookie, RequestContext};
use rustdm::testing::fake_releases::{publish_ytdlp, test_sources};
use rustdm::testing::video::{
    recorded_args, recorded_cookies, test_tool_program_scripted, video_page_json, write_script,
    youtube_like, PageSpec, ToolResponse, ToolScript, VideoWorld,
};
use rustdm::testing::{pattern_bytes, Mode, Route, TestServer};
use rustdm::tools::{FetchCtx, Freshness, Tool, ToolStore};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const NO_VIDEO: &str = "This link is a web page, not a file, and no video was found on it.";
const LOOKING: &str = "Looking for a video on this page…";
/// The streams a chosen quality of `Setup::page` fetches from the page server.
const MEDIA: [&str; 3] = ["/media/136", "/media/137", "/media/140"];
const NEWER_YTDLP: &str = "2026.11.14";

struct Setup {
    world: VideoWorld,
    pages: TestServer,
    data: tempfile::TempDir,
    folder: tempfile::TempDir,
    core: DaemonCore,
}

fn html_page() -> Route {
    Route::ok("<html><body>a page</body></html>").with_header("Content-Type", "text/html")
}

async fn page_server() -> TestServer {
    let pages = [
        "/watch", "/watch1", "/watch2", "/watch3", "/watch4", "/watch5",
    ]
    .map(|path| (path.to_string(), html_page()));
    let media = MEDIA.map(|path| (path.to_string(), Route::ok(pattern_bytes(4096))));
    TestServer::start_routes([pages.to_vec(), media.to_vec()].concat()).await
}

async fn setup() -> Setup {
    let world = VideoWorld::new().await;
    let pages = page_server().await;
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(Arc::clone(&world.store));
    Setup {
        world,
        pages,
        data,
        folder,
        core,
    }
}

fn base() -> Url {
    Url::parse("https://video.test/").unwrap()
}

fn page_answer(spec: &PageSpec) -> ToolResponse {
    ToolResponse {
        stdout: video_page_json(spec),
        ..ToolResponse::default()
    }
}

fn error_answer(stderr: &str) -> ToolResponse {
    ToolResponse {
        stderr: stderr.to_string(),
        exit_code: 1,
        ..ToolResponse::default()
    }
}

fn script(responses: Vec<ToolResponse>) -> ToolScript {
    ToolScript { responses }
}

impl Setup {
    fn answer(&self, response: ToolResponse) {
        write_script(&self.world.ytdlp.exe, &script(vec![response]));
    }

    fn params(&self, path: &str) -> AddDownloadParams {
        AddDownloadParams {
            url: self.pages.url().join(path).unwrap().to_string(),
            destination: Some(self.folder.path().to_path_buf()),
            filename: None,
            connections: Some(2),
            max_bytes_per_sec: None,
            context: None,
            quality: None,
            added_by: None,
        }
    }

    fn add(&self, path: &str) -> DownloadSummary {
        self.core.add_download(self.params(path)).unwrap()
    }

    fn lookups(&self) -> usize {
        recorded_args(&self.world.ytdlp.exe).len()
    }

    /// The YouTube-like page with its streams on the page server, so a
    /// chosen quality downloads.
    fn page(&self) -> PageSpec {
        youtube_like(&self.pages.url())
    }

    /// Requests to the page server for `paths`.
    fn requests_to(&self, paths: &[&str]) -> usize {
        let requests = self.pages.requests();
        requests
            .iter()
            .filter(|r| paths.contains(&r.path.as_str()))
            .count()
    }
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

fn lookup_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("lookup-"))
        .collect()
}

#[tokio::test]
async fn a_video_page_waits_for_a_quality() {
    let s = setup().await;
    s.answer(page_answer(&youtube_like(&base())));

    let added = s.add("/watch");
    let waiting = wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;

    let video = waiting.video.expect("a video summary");
    assert_eq!(video.title, "A test video");
    assert_eq!(video.duration_secs, Some(212));
    let labels: Vec<&str> = video.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels.first(), Some(&"2160p"));
    assert_eq!(labels.last(), Some(&"Audio only"));
    assert_eq!(video.choices[video.default.unwrap()].label, "1080p");
    assert_eq!(waiting.step, None);
    assert_eq!(waiting.filename, "A test video.mp4");
    assert_eq!(waiting.error_message, None);
    assert!(s.core.wait_until_settled(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn the_row_says_it_is_looking_while_yt_dlp_runs() {
    let s = setup().await;
    s.answer(ToolResponse {
        sleep_ms: 1000,
        ..page_answer(&youtube_like(&base()))
    });
    let mut ticks = s.core.telemetry_tx.subscribe();

    let added = s.add("/watch");

    let deadline = Instant::now() + Duration::from_secs(5);
    let tick = loop {
        s.core.tick_telemetry();
        let mut found = None;
        while let Ok(tick) = ticks.try_recv() {
            if tick.id == added.id && tick.step.is_some() {
                found = Some(tick);
            }
        }
        if let Some(tick) = found {
            break tick;
        }
        assert!(Instant::now() < deadline, "no tick carried a step");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(tick.step.as_deref(), Some(LOOKING));
    assert_eq!(
        s.core.get_status(&added.id).unwrap().step.as_deref(),
        Some(LOOKING)
    );
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;
}

#[tokio::test]
async fn a_page_with_no_video_fails_with_the_no_video_message() {
    let s = setup().await;
    s.answer(error_answer(
        "ERROR: Unsupported URL: http://127.0.0.1/watch",
    ));

    let added = s.add("/watch");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(failed.error_message.as_deref(), Some(NO_VIDEO));
    assert_eq!(failed.step, None);
}

#[tokio::test]
async fn a_web_page_with_cookies_and_no_video_keeps_the_sign_in_message() {
    let s = setup().await;
    s.answer(error_answer(
        "ERROR: Unsupported URL: http://127.0.0.1/watch",
    ));
    let mut params = s.params("/watch");
    params.context = Some(cookie_context());

    let added = s.core.add_download(params).unwrap();
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some(web_page_with_cookies_message().as_str())
    );
    // The cookies went to yt-dlp, and are gone from the disk as they are today.
    assert_eq!(recorded_cookies(&s.world.ytdlp.exe).len(), 1);
    assert!(lookup_files(s.data.path()).is_empty());
    assert!(!s
        .data
        .path()
        .join("secrets")
        .join(format!("{}.cookies", added.id))
        .exists());
}

/// What `/watch` serves once the page has become a plain file.
fn file_route() -> Route {
    Route::ok(rustdm::testing::pattern_bytes(4096))
        .with_header("Content-Type", "application/octet-stream")
}

#[tokio::test]
async fn a_fresh_sign_in_after_no_video_checks_the_link_and_downloads_the_file() {
    let s = setup().await;
    s.answer(error_answer(
        "ERROR: Unsupported URL: http://127.0.0.1/watch",
    ));
    let mut params = s.params("/watch");
    params.context = Some(cookie_context());
    let added = s.core.add_download(params.clone()).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

    // The user signs in again; the link now gives the file.
    s.pages.set_route("/watch", file_route());
    let again = s.core.add_download(params).unwrap();
    let done = wait_for(&s.core, &again.id, DownloadStatus::Completed).await;

    assert_eq!(again.id, added.id, "the stopped download carried on");
    assert_eq!(done.error_message, None);
    assert_eq!(s.lookups(), 1, "no second lookup");
}

#[tokio::test]
async fn resuming_after_no_video_checks_the_link_again() {
    let s = setup().await;
    s.answer(error_answer(
        "ERROR: Unsupported URL: http://127.0.0.1/watch",
    ));
    let added = s.add("/watch");
    let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;
    assert_eq!(failed.error_message.as_deref(), Some(NO_VIDEO));

    // A Wi-Fi login page is gone: the link now gives the file.
    s.pages.set_route("/watch", file_route());
    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(s.lookups(), 1, "no second lookup");
}

#[tokio::test]
async fn live_drm_and_playlist_pages_say_why() {
    let s = setup().await;
    let live = PageSpec {
        is_live: true,
        ..youtube_like(&base())
    };
    let mut drm = youtube_like(&base());
    drm.formats.iter_mut().for_each(|f| f.has_drm = true);
    let playlist = PageSpec {
        playlist_count: Some(3),
        ..youtube_like(&base())
    };
    let cases = [
        (
            live,
            "This is a live stream; rustdm downloads finished videos only.",
        ),
        (
            drm,
            "This video is copy-protected; rustdm can't download it.",
        ),
        (
            playlist,
            "This is a playlist of 3 videos; playlists come in a later version.",
        ),
    ];

    for (index, (spec, message)) in cases.into_iter().enumerate() {
        s.answer(page_answer(&spec));
        let path = format!("/watch{}", index + 1);
        let added = s.add(&path);

        let failed = wait_for(&s.core, &added.id, DownloadStatus::Failed).await;

        assert_eq!(failed.error_message.as_deref(), Some(message), "{path}");
    }
}

#[tokio::test]
async fn an_extractor_error_updates_yt_dlp_once_and_tries_again() {
    let s = setup().await;
    let extractor_error =
        || error_answer("ERROR: [youtube] testVideo01: Unable to extract initial player response");
    s.answer(extractor_error());
    // The newer yt-dlp carries its own script: it answers the page first.
    let newer = script(vec![page_answer(&youtube_like(&base()))]);
    publish_ytdlp(
        &s.world.server,
        NEWER_YTDLP,
        &test_tool_program_scripted(NEWER_YTDLP, &newer),
    );

    let first = s.add("/watch");
    wait_for(&s.core, &first.id, DownloadStatus::Choosing).await;

    let installed = s
        .world
        .store
        .ensure(Tool::YtDlp, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();
    assert_eq!(installed.version, NEWER_YTDLP);
    assert_eq!(s.lookups(), 1, "the old yt-dlp ran once");
    assert_eq!(
        recorded_args(&installed.exe).len(),
        1,
        "the new one ran once"
    );

    // Now the newer one fails too: no second check within the hour.
    write_script(&installed.exe, &script(vec![extractor_error()]));
    let checks_before = s.world.server.requests().len();
    let second = s.add("/watch2");
    let failed = wait_for(&s.core, &second.id, DownloadStatus::Failed).await;

    assert_eq!(s.world.server.requests().len(), checks_before);
    assert_eq!(
        failed.error_message.as_deref(),
        Some(
            format!(
                "yt-dlp {NEWER_YTDLP} couldn't read this page; the site may have changed. \
                 rustdm will try again with the next yt-dlp."
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn cancel_during_the_lookup_stops_yt_dlp_and_removes_the_cookies_file() {
    let s = setup().await;
    s.answer(ToolResponse {
        sleep_ms: 5000,
        ..page_answer(&youtube_like(&base()))
    });
    let mut params = s.params("/watch");
    params.context = Some(cookie_context());
    let added = s.core.add_download(params).unwrap();
    // Wait until yt-dlp is running with the cookies file.
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.core.get_status(&added.id).unwrap().step.as_deref() != Some(LOOKING) || s.lookups() != 1
    {
        assert!(Instant::now() < deadline, "the lookup never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let started = Instant::now();
    s.core
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: false,
        })
        .unwrap();
    let settled = s.core.wait_until_settled(Duration::from_secs(1)).await;

    assert!(settled, "still running after the cancel");
    assert!(started.elapsed() < Duration::from_secs(1));
    let summary = s.core.get_status(&added.id).unwrap();
    assert_eq!(summary.status, DownloadStatus::Cancelled);
    assert_eq!(summary.error_message, None);
    assert_eq!(summary.step, None);
    assert_eq!(
        recorded_cookies(&s.world.ytdlp.exe).len(),
        1,
        "yt-dlp saw a cookies file while it ran"
    );
    assert!(lookup_files(s.data.path()).is_empty());
    assert_eq!(s.lookups(), 1, "yt-dlp was started and not started again");
}

#[tokio::test]
async fn no_yt_dlp_and_no_network_says_so_and_retries() {
    let down = TestServer::start(Vec::new(), Mode::Status(503)).await;
    let tools_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ToolStore::with_sources(
            tools_dir.path().join("tools"),
            build_client(),
            test_sources(&down),
        )
        .unwrap(),
    );
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path()))
        .unwrap()
        .with_tools(store);
    let pages = page_server().await;
    let params = AddDownloadParams {
        url: pages.url().join("/watch").unwrap().to_string(),
        destination: Some(folder.path().to_path_buf()),
        filename: None,
        connections: Some(2),
        max_bytes_per_sec: None,
        context: None,
        quality: None,
        added_by: None,
    };

    let added = core.add_download(params).unwrap();
    let retrying = wait_for(&core, &added.id, DownloadStatus::Retrying).await;

    let message = retrying.error_message.unwrap();
    assert!(
        message.starts_with("Couldn't reach github.com to get yt-dlp; will retry; retry 1 of"),
        "{message}"
    );
}

const TIMED_OUT_STDERR: &str =
    "ERROR: [youtube] testVideo01: Unable to download webpage: <urlopen error timed out>";
const TIMED_OUT_MESSAGE: &str =
    "[youtube] testVideo01: Unable to download webpage: <urlopen error timed out>";

/// Bring the retry of a waiting download forward and start it.
fn retry_now(core: &DaemonCore, id: &str) {
    {
        let state = core.state.lock().unwrap();
        state.downloads[id].lock().unwrap().retry_at = Some(Instant::now());
    }
    core.retry_due_downloads();
}

/// Retry a download that keeps failing in the lookup until it gives up.
async fn retry_until_it_fails(core: &DaemonCore, id: &str) -> DownloadSummary {
    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let summary = loop {
            let summary = core.get_status(id).unwrap();
            if matches!(
                summary.status,
                DownloadStatus::Retrying | DownloadStatus::Failed
            ) {
                break summary;
            }
            assert!(Instant::now() < deadline, "{id} is {:?}", summary.status);
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        if summary.status == DownloadStatus::Failed {
            return summary;
        }
        retry_now(core, id);
    }
}

#[tokio::test]
async fn a_lookup_that_failed_for_a_moment_is_tried_again_without_the_link_check() {
    let s = setup().await;
    write_script(
        &s.world.ytdlp.exe,
        &script(vec![
            error_answer(TIMED_OUT_STDERR),
            page_answer(&youtube_like(&base())),
        ]),
    );

    let added = s.add("/watch");
    wait_for(&s.core, &added.id, DownloadStatus::Retrying).await;
    let page_checks = s.pages.requests().len();
    assert!(page_checks > 0, "the first run checked the link");
    retry_now(&s.core, &added.id);
    let waiting = wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;

    assert_eq!(s.lookups(), 2);
    assert_eq!(s.pages.requests().len(), page_checks, "one link check");
    assert_eq!(waiting.error_message, None);
}

#[tokio::test]
async fn a_lookup_that_keeps_failing_ends_with_its_own_message() {
    let s = setup().await;
    s.answer(error_answer(TIMED_OUT_STDERR));

    let added = s.add("/watch");
    wait_for(&s.core, &added.id, DownloadStatus::Retrying).await;
    let page_checks = s.pages.requests().len();
    let failed = retry_until_it_fails(&s.core, &added.id).await;

    assert_eq!(failed.error_message.as_deref(), Some(TIMED_OUT_MESSAGE));
    assert_eq!(s.lookups(), 1 + MAX_AUTO_RETRIES as usize);
    assert_eq!(s.pages.requests().len(), page_checks, "one link check");

    // The user's resume goes to the lookup too.
    s.answer(page_answer(&youtube_like(&base())));
    s.core.resume_download(&added.id).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;
    assert_eq!(s.lookups(), 1);
    assert_eq!(s.pages.requests().len(), page_checks, "no link check");
}

#[tokio::test]
async fn a_lookup_that_keeps_failing_keeps_the_sign_in() {
    let s = setup().await;
    s.answer(error_answer(TIMED_OUT_STDERR));
    let mut params = s.params("/watch");
    params.context = Some(cookie_context());

    let added = s.core.add_download(params).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Retrying).await;
    let failed = retry_until_it_fails(&s.core, &added.id).await;

    assert_eq!(failed.error_message.as_deref(), Some(TIMED_OUT_MESSAGE));
    assert_ne!(
        failed.error_message.as_deref(),
        Some(web_page_with_cookies_message().as_str())
    );
    let secret = s
        .data
        .path()
        .join("secrets")
        .join(format!("{}.cookies", added.id));
    assert!(secret.exists(), "the sign-in was kept");
    assert_eq!(
        recorded_cookies(&s.world.ytdlp.exe).len(),
        1 + MAX_AUTO_RETRIES as usize,
        "every lookup carried the cookies"
    );
}

#[tokio::test]
async fn a_bad_quality_is_refused_at_add() {
    let s = setup().await;
    let mut params = s.params("/watch");
    params.quality = Some("hd".to_string());

    let err = s.core.add_download(params).unwrap_err();

    assert_eq!(err.code, CODE_INVALID_PARAMS);
    assert_eq!(
        err.message,
        "Invalid params: quality must be best, audio or a height such as 1080p"
    );
    assert!(s.core.list_downloads().is_empty());
}

#[tokio::test]
async fn without_a_tool_store_web_pages_fail_as_before() {
    let pages = page_server().await;
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::new();
    let added = core
        .add_download(AddDownloadParams {
            url: pages.url().join("/watch").unwrap().to_string(),
            destination: Some(folder.path().to_path_buf()),
            filename: None,
            connections: Some(2),
            max_bytes_per_sec: None,
            context: None,
            quality: None,
            added_by: None,
        })
        .unwrap();

    let failed = wait_for(&core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some("the link opened a web page, not a file")
    );
}

#[tokio::test]
async fn a_quality_given_at_add_is_chosen_at_once() {
    let s = setup().await;
    s.answer(page_answer(&s.page()));
    let mut params = s.params("/watch");
    params.quality = Some("720".to_string());

    let added = s.core.add_download(params).unwrap();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert!(s.requests_to(&["/media/136"]) > 0, "the video stream ran");
    let video = done.video.expect("a video summary");
    let chosen = video.chosen.expect("a chosen quality");
    assert_eq!(chosen.label, "720p");
    assert_eq!(chosen.choice_id, "720-mp4");
    assert!(video.choices.is_empty());
    assert_eq!(done.filename, "A test video.mp4");
}

#[tokio::test]
async fn a_name_asked_for_at_add_names_the_video_file() {
    let s = setup().await;
    s.answer(page_answer(&s.page()));
    let mut waits = s.params("/watch");
    waits.filename = Some("My clip".to_string());
    let mut chooses = s.params("/watch1");
    chooses.filename = Some("My clip".to_string());
    chooses.quality = Some("720".to_string());

    let waiting = s.core.add_download(waits).unwrap();
    let waiting = wait_for(&s.core, &waiting.id, DownloadStatus::Choosing).await;
    let chosen = s.core.add_download(chooses).unwrap();
    let chosen = wait_for(&s.core, &chosen.id, DownloadStatus::Completed).await;

    assert_eq!(waiting.filename, "My clip.mp4");
    assert_eq!(chosen.filename, "My clip.mp4");
}

#[tokio::test]
async fn a_cancelled_choice_comes_back_without_a_new_lookup_or_link_check() {
    let s = setup().await;
    s.answer(page_answer(&youtube_like(&base())));
    let added = s.add("/watch");
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;
    s.core
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: false,
        })
        .unwrap();
    let requests = s.pages.requests().len();

    s.core.resume_download(&added.id).unwrap();
    let back = wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;

    assert_eq!(back.video.unwrap().choices.len(), 7);
    assert_eq!(s.lookups(), 1);
    assert_eq!(s.pages.requests().len(), requests);
}

#[tokio::test]
async fn a_choice_waits_across_a_restart() {
    let s = setup().await;
    s.answer(page_answer(&youtube_like(&base())));
    let added = s.add("/watch");
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;
    s.core.wait_until_settled(Duration::from_secs(5)).await;

    let again = DaemonCore::with_store(QueueStore::in_dir(s.data.path())).unwrap();
    again.schedule();

    let summary = again.get_status(&added.id).unwrap();
    assert_eq!(summary.status, DownloadStatus::Choosing);
    assert_eq!(summary.video.unwrap().title, "A test video");
}

#[tokio::test]
async fn without_a_data_folder_the_lookup_runs_without_cookies() {
    let world = VideoWorld::new().await;
    write_script(
        &world.ytdlp.exe,
        &script(vec![page_answer(&youtube_like(&base()))]),
    );
    let pages = page_server().await;
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::new().with_tools(Arc::clone(&world.store));
    let added = core
        .add_download(AddDownloadParams {
            url: pages.url().join("/watch").unwrap().to_string(),
            destination: Some(folder.path().to_path_buf()),
            filename: None,
            connections: Some(2),
            max_bytes_per_sec: None,
            context: Some(cookie_context()),
            quality: None,
            added_by: None,
        })
        .unwrap();

    wait_for(&core, &added.id, DownloadStatus::Choosing).await;

    assert!(recorded_cookies(&world.ytdlp.exe).is_empty());
    assert_eq!(recorded_args(&world.ytdlp.exe).len(), 1);
}

#[tokio::test]
async fn stale_cookie_files_are_swept_when_the_daemon_starts() {
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("lookup-00ff.txt"), "# cookies").unwrap();
    std::fs::write(data.path().join("lookup-0a0b.txt"), "# cookies").unwrap();
    std::fs::write(data.path().join("queue.json.keep"), "x").unwrap();
    std::fs::write(data.path().join("lookup-notes.json"), "x").unwrap();

    let _core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();

    assert_eq!(lookup_files(data.path()), vec!["lookup-notes.json"]);
    assert!(data.path().join("queue.json.keep").exists());
}

fn choose(id: &str, choice_id: &str) -> ChooseQualityParams {
    ChooseQualityParams {
        id: id.to_string(),
        choice_id: choice_id.to_string(),
        destination: None,
        filename: None,
    }
}

/// A download of the stand-in video page that waits for its quality.
async fn choosing(s: &Setup, path: &str) -> DownloadSummary {
    s.answer(page_answer(&s.page()));
    let added = s.add(path);
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await
}

#[tokio::test]
async fn choosing_a_quality_stores_the_plan() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;

    let chosen = s
        .core
        .choose_quality(choose(&waiting.id, "720-mp4"))
        .unwrap();

    // Chosen, it waits for a slot like any download, then runs its streams.
    assert_eq!(chosen.status, DownloadStatus::Queued);
    let ended = wait_for(&s.core, &waiting.id, DownloadStatus::Completed).await;
    for summary in [chosen, ended] {
        assert_eq!(summary.filename, "A test video.mp4");
        let video = summary.video.expect("a video summary");
        assert_eq!(video.chosen.expect("a chosen quality").label, "720p");
        assert!(video.choices.is_empty());
    }
    assert!(s.core.wait_until_settled(Duration::from_secs(5)).await);
    let again = DaemonCore::with_store(QueueStore::in_dir(s.data.path())).unwrap();
    let kept = again.get_status(&waiting.id).unwrap();
    assert_eq!(kept.status, DownloadStatus::Completed);
    assert_eq!(kept.video.unwrap().chosen.unwrap().choice_id, "720-mp4");
}

#[tokio::test]
async fn a_choice_can_move_the_download_and_name_the_file() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;
    let elsewhere = tempfile::tempdir().unwrap();
    let mut params = choose(&waiting.id, "720-mp4");
    params.destination = Some(elsewhere.path().to_path_buf());
    params.filename = Some("My clip".to_string());

    let chosen = s.core.choose_quality(params).unwrap();

    assert_eq!(chosen.filename, "My clip.mp4");
    assert_eq!(chosen.destination, elsewhere.path());
}

#[tokio::test]
async fn a_quality_given_at_add_skips_the_question() {
    let s = setup().await;
    s.answer(page_answer(&s.page()));
    let mut params = s.params("/watch");
    params.quality = Some("1080p".to_string());

    let added = s.core.add_download(params).unwrap();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.video.unwrap().chosen.unwrap().label, "1080p");
}

#[tokio::test]
async fn a_missing_height_takes_the_nearest_below() {
    let s = setup().await;
    s.answer(page_answer(&s.page()));
    let mut params = s.params("/watch");
    params.quality = Some("900p".to_string());

    let added = s.core.add_download(params).unwrap();
    let done = wait_for(&s.core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(done.video.unwrap().chosen.unwrap().label, "720p");
}

#[tokio::test]
async fn choose_refuses_a_download_that_is_not_waiting() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;
    s.core
        .choose_quality(choose(&waiting.id, "720-mp4"))
        .unwrap();

    let err = s
        .core
        .choose_quality(choose(&waiting.id, "1080-mp4"))
        .unwrap_err();

    assert_eq!(err.code, CODE_INVALID_PARAMS);
    assert_eq!(
        err.message,
        "Invalid params: This download isn't waiting for a quality"
    );
    let missing = s
        .core
        .choose_quality(choose("dl_999", "720-mp4"))
        .unwrap_err();
    assert_ne!(missing.code, CODE_INVALID_PARAMS);
}

#[tokio::test]
async fn choose_refuses_an_unknown_quality() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;

    let err = s
        .core
        .choose_quality(choose(&waiting.id, "8k-av1"))
        .unwrap_err();

    assert_eq!(err.code, CODE_INVALID_PARAMS);
    assert_eq!(
        err.message,
        "Invalid params: No such quality for this video"
    );
    assert_eq!(
        s.core.get_status(&waiting.id).unwrap().status,
        DownloadStatus::Choosing
    );
}

#[tokio::test]
async fn choose_checks_the_folder_and_name() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;
    let mut bad_folder = choose(&waiting.id, "720-mp4");
    bad_folder.destination = Some("not/a/full/path".into());
    let mut bad_name = choose(&waiting.id, "720-mp4");
    bad_name.filename = Some("..".to_string());

    let folder_err = s.core.choose_quality(bad_folder).unwrap_err();
    let name_err = s.core.choose_quality(bad_name).unwrap_err();

    assert_eq!(folder_err.code, CODE_INVALID_PATH);
    assert!(folder_err.message.contains("full path"), "{folder_err:?}");
    assert_eq!(name_err.code, CODE_INVALID_PARAMS);
    assert_eq!(
        name_err.message,
        "Invalid params: \"..\" is not a usable file name"
    );
    let still = s.core.get_status(&waiting.id).unwrap();
    assert_eq!(still.status, DownloadStatus::Choosing);
    assert_eq!(still.destination, s.folder.path());
}

#[tokio::test]
async fn choose_quality_is_routed_by_its_method_name() {
    let s = setup().await;
    let waiting = choosing(&s, "/watch").await;
    let params = serde_json::to_value(choose(&waiting.id, "720-mp4")).unwrap();

    let response = s.core.handle_request(RpcRequest::new(
        RpcId::Number(1),
        "download.choose_quality".to_string(),
        Some(params),
    ));

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value["result"]["video"]["chosen"]["label"], "720p",
        "{value}"
    );
}

#[tokio::test]
async fn resuming_a_chosen_video_needs_no_new_lookup_or_link_check() {
    let s = setup().await;
    // The first join fails, so the download waits for the user's resume.
    let fails = ToolResponse {
        stderr: "Invalid data found\n".to_string(),
        exit_code: 1,
        join_inputs: true,
        ..ToolResponse::default()
    };
    let joins = ToolResponse {
        join_inputs: true,
        ..ToolResponse::default()
    };
    write_script(&s.world.ffmpeg.exe, &script(vec![fails, joins]));
    let waiting = choosing(&s, "/watch").await;
    s.core
        .choose_quality(choose(&waiting.id, "720-mp4"))
        .unwrap();
    wait_for(&s.core, &waiting.id, DownloadStatus::Failed).await;
    let pages = s.requests_to(&["/watch"]);
    let streams = s.requests_to(&MEDIA);

    s.core.resume_download(&waiting.id).unwrap();
    assert!(s.core.wait_until_settled(Duration::from_secs(5)).await);

    let again = s.core.get_status(&waiting.id).unwrap();
    assert_eq!(again.status, DownloadStatus::Completed);
    assert_eq!(s.lookups(), 1);
    assert_eq!(s.requests_to(&["/watch"]), pages);
    // Both streams were already there.
    assert_eq!(s.requests_to(&MEDIA), streams);
}

#[tokio::test]
async fn choosing_a_quality_keeps_its_streams_and_no_cookies() {
    let s = setup().await;
    let mut spec = s.page();
    for format in &mut spec.formats {
        format.http_headers.extend(
            [
                ("Cookie", "sid=abc"),
                ("Authorization", "Bearer abc"),
                ("X-Foo", "bar"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string())),
        );
    }
    s.answer(page_answer(&spec));
    let mut params = s.params("/watch");
    params.context = Some(cookie_context());
    let added = s.core.add_download(params).unwrap();
    wait_for(&s.core, &added.id, DownloadStatus::Choosing).await;

    s.core
        .choose_quality(choose(&added.id, "1080-mp4"))
        .unwrap();

    wait_for(&s.core, &added.id, DownloadStatus::Completed).await;
    assert!(s.core.wait_until_settled(Duration::from_secs(5)).await);
    let text = std::fs::read_to_string(s.data.path().join("queue.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
    let plan = &saved["downloads"][0]["video"];
    let streams = plan["streams"].as_array().unwrap();
    let ids: Vec<&str> = streams
        .iter()
        .map(|s| s["format_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["137", "140"]);
    let kinds: Vec<&str> = streams
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["video", "audio"]);
    let media = |path: &str| s.pages.url().join(path).unwrap().to_string();
    assert_eq!(streams[0]["url"], media("/media/137"));
    assert_eq!(streams[1]["url"], media("/media/140"));
    assert_eq!(streams[0]["size"], 1080 * 40_000);
    assert_eq!(streams[1]["size"], 3_000_000);
    assert_eq!(streams[0]["ext"], "mp4");
    assert_eq!(streams[1]["ext"], "m4a");
    for stream in streams {
        assert_eq!(stream["request_bytes"], 10_485_760);
        let names: Vec<&str> = stream["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| pair[0].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["Accept", "Accept-Language", "Sec-Fetch-Mode", "User-Agent"]
        );
    }
    assert_eq!(plan["offered"], serde_json::json!([]));
    assert_eq!(plan["choices"], serde_json::json!([]));
    assert!(
        !text.contains("abc"),
        "a cookie value reached the queue file"
    );
}
