//! The tools manager end to end against a pretend release host. Nothing here
//! touches the internet.

use rustdm::engine::build_client;
use rustdm::ipc::protocol::ToolPhase;
use rustdm::testing::fake_releases::*;
use rustdm::testing::{Mode, RequestLog, Route, TestServer};
use rustdm::tools::checksum::sha256_hex;
use rustdm::tools::pins::Pin;
use rustdm::tools::{FetchCtx, Freshness, Tool, ToolError, ToolStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const V1: &str = "2026.11.10.100000";
const V2: &str = "2026.11.14.200000";
const V3: &str = "2026.11.15.300000";

struct World {
    server: TestServer,
    dir: tempfile::TempDir,
}

impl World {
    async fn new() -> World {
        World {
            server: TestServer::start_routes(vec![]).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("tools")
    }

    /// A store as a freshly started daemon would open it.
    fn store(&self) -> Arc<ToolStore> {
        Arc::new(
            ToolStore::with_sources(self.root(), build_client(), test_sources(&self.server))
                .unwrap(),
        )
    }

    fn requests_for(&self, path: &str) -> Vec<RequestLog> {
        self.server
            .requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }

    fn api_calls(&self) -> usize {
        self.requests_for(LATEST_PATH).len()
    }

    fn tools_json(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.root().join("tools.json")).unwrap()).unwrap()
    }
}

async fn ensure_now(store: &ToolStore, tool: Tool) -> Result<rustdm::tools::Installed, ToolError> {
    store
        .ensure(tool, Freshness::CheckNow, FetchCtx::new())
        .await
}

fn version_folders(root: &Path, tool: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join(tool))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn info(store: &ToolStore, tool: Tool) -> rustdm::ipc::protocol::ToolInfo {
    store
        .status()
        .into_iter()
        .find(|i| Tool::from(i.tool) == tool)
        .unwrap()
}

#[tokio::test]
async fn first_use_downloads_verifies_and_installs_ytdlp() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();

    let installed = store
        .ensure(Tool::YtDlp, Freshness::CheckIfStale, FetchCtx::new())
        .await
        .unwrap();

    assert_eq!(installed.version, V1);
    assert!(installed.exe.is_file());
    assert_eq!(installed.exe.parent().unwrap(), installed.dir);
    assert!(installed
        .exe
        .starts_with(world.root().join("yt-dlp").join(V1)));
    assert_eq!(world.tools_json()["tools"]["yt-dlp"]["current"], V1);
    assert_eq!(
        world.tools_json()["tools"]["yt-dlp"]["sha256"],
        sha256_hex(&std::fs::read(&installed.exe).unwrap())
    );
    let shown = info(&store, Tool::YtDlp);
    assert_eq!(shown.installed.as_deref(), Some(V1));
    assert_eq!(shown.error, None);
    assert!(shown.last_checked_unix.is_some());
    // Nothing is left behind in staging.
    assert!(!world
        .root()
        .join(".staging")
        .join(format!("yt-dlp-{V1}"))
        .exists());
}

#[tokio::test]
async fn an_installed_tool_is_used_without_asking_again() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();
    let calls = world.server.requests().len();

    for fresh in [Freshness::AsInstalled, Freshness::CheckIfStale] {
        let again = store
            .ensure(Tool::YtDlp, fresh, FetchCtx::new())
            .await
            .unwrap();
        assert_eq!(again.version, V1);
    }
    assert_eq!(world.server.requests().len(), calls, "no network use");
}

#[tokio::test]
async fn two_callers_at_once_fetch_only_once() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();

    let (a, b) = tokio::join!(
        store.ensure(Tool::YtDlp, Freshness::CheckIfStale, FetchCtx::new()),
        store.ensure(Tool::YtDlp, Freshness::CheckIfStale, FetchCtx::new()),
    );

    assert_eq!(a.unwrap().version, V1);
    assert_eq!(b.unwrap().version, V1);
    assert_eq!(world.api_calls(), 1);
    let asset = format!("/{TEST_REPO}/releases/download/{V1}/{TEST_ASSET}");
    let single_run_requests = world.requests_for(&asset).len();
    assert!(single_run_requests > 0);

    // The same download run alone makes as many requests, so nothing was fetched twice.
    let alone = World::new().await;
    publish_ytdlp(&alone.server, V1, &fake_program(V1));
    ensure_now(&alone.store(), Tool::YtDlp).await.unwrap();
    assert_eq!(alone.requests_for(&asset).len(), single_run_requests);
}

#[tokio::test]
async fn an_update_keeps_the_previous_version_and_rollback_goes_back() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    publish_ytdlp(&world.server, V2, &fake_program(V2));
    let updated = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(updated.version, V2);
    let shown = info(&store, Tool::YtDlp);
    assert_eq!(shown.installed.as_deref(), Some(V2));
    assert_eq!(shown.previous.as_deref(), Some(V1));
    assert_eq!(version_folders(&world.root(), "yt-dlp"), vec![V1, V2]);

    let rolled = store.rollback(Tool::YtDlp).unwrap();
    assert_eq!(rolled.installed.as_deref(), Some(V1));
    assert_eq!(rolled.previous.as_deref(), Some(V2));
    assert_eq!(world.tools_json()["tools"]["yt-dlp"]["skip_version"], V2);

    // The version rolled back from is not installed again by itself.
    let still = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(still.version, V1);

    // A newer one is; the old previous is cleaned up.
    publish_ytdlp(&world.server, V3, &fake_program(V3));
    let newest = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(newest.version, V3);
    assert_eq!(info(&store, Tool::YtDlp).previous.as_deref(), Some(V1));
    assert_eq!(version_folders(&world.root(), "yt-dlp"), vec![V1, V3]);
}

/// A speed per connection at which `program` downloads in about two seconds
/// over the tools' 4 connections. On Windows a fake program is a copy of the
/// test build of rdmd, megabytes where the Unix script is a few bytes, so a
/// fixed speed made it take most of a test's 30-second wait there.
fn throttle_for_two_seconds(program: &[u8]) -> u64 {
    (program.len() as u64 / 8).max(150_000)
}

async fn wait_until_idle(store: &ToolStore, tool: Tool) {
    let started = std::time::Instant::now();
    while info(store, tool).phase != ToolPhase::Idle {
        assert!(started.elapsed() < Duration::from_secs(30), "still busy");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_user_can_update_to_a_version_they_rolled_back_from() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();
    publish_ytdlp(&world.server, V2, &fake_program(V2));
    ensure_now(&store, Tool::YtDlp).await.unwrap();
    store.rollback(Tool::YtDlp).unwrap();
    assert_eq!(world.tools_json()["tools"]["yt-dlp"]["skip_version"], V2);
    assert_eq!(ensure_now(&store, Tool::YtDlp).await.unwrap().version, V1);

    store.start_update(Some(Tool::YtDlp));
    wait_until_idle(&store, Tool::YtDlp).await;

    let shown = info(&store, Tool::YtDlp);
    assert_eq!(shown.installed.as_deref(), Some(V2));
    assert_eq!(shown.previous.as_deref(), Some(V1));
    assert_eq!(shown.error, None);
    assert!(world.tools_json()["tools"]["yt-dlp"]
        .get("skip_version")
        .is_none());
}

#[tokio::test]
async fn a_release_that_reports_another_version_than_its_tag_is_refused() {
    // An old, genuinely signed release published under a future tag.
    let world = World::new().await;
    publish_ytdlp(&world.server, "9999.12.31", &fake_program(V1));
    let store = world.store();

    let err = ensure_now(&store, Tool::YtDlp).await.unwrap_err();

    assert!(matches!(err, ToolError::VersionMismatch { .. }), "{err}");
    assert!(!err.is_transient());
    assert!(version_folders(&world.root(), "yt-dlp").is_empty());
    // It does not block the real release that comes after it.
    publish_ytdlp(&world.server, V2, &fake_program(V2));
    assert_eq!(ensure_now(&store, Tool::YtDlp).await.unwrap().version, V2);

    // With a copy installed, the copy stays and the reason shows.
    publish_ytdlp(&world.server, "9999.12.31", &fake_program(V1));
    let kept = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(kept.version, V2);
    let note = info(&store, Tool::YtDlp).error.unwrap();
    assert!(note.contains("reports itself as"), "{note}");
    assert_eq!(world.tools_json()["tools"]["yt-dlp"]["current"], V2);
}

#[tokio::test]
async fn a_release_older_than_the_floor_is_never_installed() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let mut sources = test_sources(&world.server);
    sources.ytdlp_min_version = "2026.11.12".to_string();
    let store = ToolStore::with_sources(world.root(), build_client(), sources).unwrap();

    let err = ensure_now(&store, Tool::YtDlp).await.unwrap_err();

    assert!(
        err.to_string().contains("older than the oldest version"),
        "{err}"
    );
    assert!(version_folders(&world.root(), "yt-dlp").is_empty());
}

#[tokio::test]
async fn an_update_signed_no_later_than_the_installed_one_is_refused() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();
    let signed = world.tools_json()["tools"]["yt-dlp"]["signed_at"]
        .as_u64()
        .expect("the signing time is stored");

    // A newer-looking release whose signature is older than the installed one's.
    publish_ytdlp_with(
        &world.server,
        ReleaseSpec {
            version: V2.to_string(),
            asset: fake_program(V2),
            listed_sha256: None,
            tamper_after_signing: false,
            signed_at: Some(signed as u32),
        },
    );
    let kept = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(kept.version, V1);
    let note = info(&store, Tool::YtDlp).error.unwrap();
    assert!(note.contains("signed no later than"), "{note}");

    // A file from before the time was recorded accepts any signature time.
    let mut json = world.tools_json();
    json["tools"]["yt-dlp"]
        .as_object_mut()
        .unwrap()
        .remove("signed_at");
    std::fs::write(world.root().join("tools.json"), json.to_string()).unwrap();
    let store = world.store();
    assert_eq!(ensure_now(&store, Tool::YtDlp).await.unwrap().version, V2);
    assert_eq!(world.tools_json()["tools"]["yt-dlp"]["signed_at"], signed);
}

#[tokio::test]
async fn a_rate_limit_wait_is_never_held_longer_than_an_hour() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    let in_a_day = rustdm::tools::release::now_unix() + 86_400;
    world.server.set_route(
        LATEST_PATH,
        Route::status(403)
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", &in_a_day.to_string()),
    );
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    let stored = world.tools_json()["tools"]["yt-dlp"]["retry_not_before"]
        .as_u64()
        .unwrap();
    assert!(
        stored <= rustdm::tools::release::now_unix() + 3600,
        "{stored}"
    );
}

#[tokio::test]
async fn rollback_while_an_update_runs_is_refused_and_a_second_update_is_skipped() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    let mut slow = fake_program(V2);
    slow.extend(std::iter::repeat_n(b'#', 300_000));
    publish_ytdlp(&world.server, V2, &slow);
    let asset = format!("/{TEST_REPO}/releases/download/{V2}/{TEST_ASSET}");
    world.server.set_route(
        &asset,
        Route::ok(slow.clone()).throttled(throttle_for_two_seconds(&slow)),
    );
    let before = world.api_calls();

    store.start_update(Some(Tool::YtDlp));
    let started = std::time::Instant::now();
    while info(&store, Tool::YtDlp).phase != ToolPhase::Downloading {
        assert!(started.elapsed() < Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let err = store.rollback(Tool::YtDlp).unwrap_err();
    assert!(err.to_string().contains("is being updated"), "{err}");
    let again = store.start_update(Some(Tool::YtDlp));
    let busy = again.iter().find(|i| i.tool == Tool::YtDlp.into()).unwrap();
    assert_ne!(busy.phase, ToolPhase::Idle);

    wait_until_idle(&store, Tool::YtDlp).await;
    assert_eq!(info(&store, Tool::YtDlp).installed.as_deref(), Some(V2));
    assert_eq!(
        world.api_calls() - before,
        1,
        "the second request was skipped"
    );
}

#[tokio::test]
async fn rollback_without_a_previous_version_says_so() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    let err = store.rollback(Tool::YtDlp).unwrap_err();
    assert!(err.to_string().contains("no earlier version"), "{err}");
}

#[tokio::test]
async fn an_older_release_never_replaces_a_newer_install() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V2, &fake_program(V2));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let kept = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(kept.version, V2);
    assert_eq!(version_folders(&world.root(), "yt-dlp"), vec![V2]);
}

#[tokio::test]
async fn a_tampered_download_leaves_no_version_folder() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    // The signed list names the real file; the server hands out something else.
    let asset = format!("/{TEST_REPO}/releases/download/{V1}/{TEST_ASSET}");
    world
        .server
        .set_route(&asset, Route::ok(fake_program("evil")));
    let store = world.store();

    let err = ensure_now(&store, Tool::YtDlp).await.unwrap_err();

    assert!(matches!(err, ToolError::ChecksumMismatch { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        format!("yt-dlp {V1} didn't match its published checksum, so it was deleted and not used.")
    );
    assert!(version_folders(&world.root(), "yt-dlp").is_empty());
    assert!(!world
        .root()
        .join(".staging")
        .join(format!("yt-dlp-{V1}"))
        .exists());
    assert_eq!(info(&store, Tool::YtDlp).installed, None);
    assert_eq!(
        info(&store, Tool::YtDlp).error.as_deref(),
        Some(err.to_string().as_str())
    );
}

#[tokio::test]
async fn a_checksum_list_signed_by_the_wrong_key_installs_nothing() {
    let world = World::new().await;
    publish_ytdlp_with(
        &world.server,
        ReleaseSpec {
            version: V1.to_string(),
            asset: fake_program(V1),
            listed_sha256: None,
            tamper_after_signing: true,
            signed_at: None,
        },
    );
    let store = world.store();

    let err = ensure_now(&store, Tool::YtDlp).await.unwrap_err();

    assert!(matches!(err, ToolError::BadSignature { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        "The yt-dlp release list isn't signed by yt-dlp's key; nothing was installed."
    );
    assert!(version_folders(&world.root(), "yt-dlp").is_empty());
    // The asset itself was never fetched.
    let asset = format!("/{TEST_REPO}/releases/download/{V1}/{TEST_ASSET}");
    assert!(world.requests_for(&asset).is_empty());
}

#[tokio::test]
async fn assets_outside_the_projects_download_path_are_refused() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let base = world
        .server
        .url()
        .as_str()
        .trim_end_matches('/')
        .to_string();
    let record = serde_json::json!({
        "tag_name": V1,
        "assets": [
            {"name": "SHA2-256SUMS", "browser_download_url": format!("{base}/elsewhere/SHA2-256SUMS")},
            {"name": "SHA2-256SUMS.sig", "browser_download_url": format!("{base}/elsewhere/SHA2-256SUMS.sig")},
            {"name": TEST_ASSET, "browser_download_url": format!("{base}/elsewhere/{TEST_ASSET}")},
        ]
    });
    world
        .server
        .set_route(LATEST_PATH, Route::ok(record.to_string()));

    let err = ensure_now(&world.store(), Tool::YtDlp).await.unwrap_err();

    assert!(matches!(err, ToolError::Unusable(_)), "{err}");
    assert!(world.requests_for("/elsewhere/SHA2-256SUMS").is_empty());
}

#[tokio::test]
async fn a_release_tag_that_is_not_a_version_is_refused() {
    let world = World::new().await;
    publish_ytdlp(&world.server, "../../escape", &fake_program(V1));
    let err = ensure_now(&world.store(), Tool::YtDlp).await.unwrap_err();
    assert!(matches!(err, ToolError::Unusable(_)), "{err}");
}

#[tokio::test]
async fn cancelling_a_download_and_trying_again_resumes_it() {
    let world = World::new().await;
    let mut program = fake_program(V1);
    program.extend(std::iter::repeat_n(b'#', 300_000));
    publish_ytdlp(&world.server, V1, &program);
    let asset = format!("/{TEST_REPO}/releases/download/{V1}/{TEST_ASSET}");
    world.server.set_route(
        &asset,
        Route::ok(program.clone()).throttled(throttle_for_two_seconds(&program)),
    );
    let store = world.store();

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let ctx = FetchCtx {
        cancel: Some(cancel_rx),
        ..FetchCtx::new()
    };
    let progress = ctx.progress.clone();
    let canceller = tokio::spawn(async move {
        while progress
            .downloaded
            .load(std::sync::atomic::Ordering::SeqCst)
            < 30_000
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel_tx.send(true).unwrap();
    });
    let err = store
        .ensure(Tool::YtDlp, Freshness::CheckNow, ctx)
        .await
        .unwrap_err();
    canceller.await.unwrap();
    assert!(matches!(err, ToolError::Cancelled), "{err}");
    assert_eq!(
        info(&store, Tool::YtDlp).phase,
        rustdm::ipc::protocol::ToolPhase::Idle
    );
    assert_eq!(info(&store, Tool::YtDlp).error, None);

    let staging = world.root().join(".staging").join(format!("yt-dlp-{V1}"));
    let kept: Vec<String> = std::fs::read_dir(&staging)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        kept.iter().any(|n| n.ends_with(".part.rdm")),
        "the download state is kept for a resume: {kept:?}"
    );

    // A new daemon run tidies the folder at startup, but not what could resume.
    let store = world.store();
    assert!(staging.join(format!("{TEST_ASSET}.part.rdm")).exists());

    let before = world.server.requests().len();
    let installed = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(installed.version, V1);
    assert_eq!(std::fs::read(&installed.exe).unwrap(), program);

    // The second run asked only for what was missing: no data request from byte 0.
    let resumed: Vec<RequestLog> = world.server.requests().split_off(before);
    let data_requests: Vec<&RequestLog> = resumed
        .iter()
        .filter(|r| r.path == asset && r.range.as_deref() != Some("bytes=0-0"))
        .collect();
    assert!(!data_requests.is_empty());
    for request in data_requests {
        let range = request
            .range
            .as_deref()
            .expect("resumed requests use Range");
        let start: u64 = range
            .trim_start_matches("bytes=")
            .split('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(start > 0, "re-fetched from the start: {range}");
    }
}

#[tokio::test]
async fn rate_limiting_falls_back_to_the_installed_copy_and_backs_off() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    let reset = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600)
        .to_string();
    world.server.set_route(
        LATEST_PATH,
        Route::status(403)
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", &reset),
    );

    let used = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(used.version, V1);
    let note = info(&store, Tool::YtDlp).error.unwrap();
    assert!(
        note.starts_with("GitHub is limiting update checks until "),
        "{note}"
    );
    assert!(note.ends_with(&format!("; using yt-dlp {V1}")), "{note}");

    // Told to wait, so it does not ask again.
    let calls = world.api_calls();
    ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(world.api_calls(), calls);
}

#[tokio::test]
async fn rate_limiting_with_nothing_installed_is_an_error() {
    let world = World::new().await;
    world.server.set_route(
        LATEST_PATH,
        Route::status(429).with_header("retry-after", "120"),
    );
    let err = ensure_now(&world.store(), Tool::YtDlp).await.unwrap_err();
    assert!(matches!(err, ToolError::RateLimited { .. }), "{err}");
    assert!(
        err.to_string().ends_with("; can't get yt-dlp until then"),
        "{err}"
    );
}

#[tokio::test]
async fn a_failed_check_with_a_copy_installed_keeps_the_copy() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    world.server.set_route(LATEST_PATH, Route::status(502));
    let used = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(used.version, V1);
    assert_eq!(
        info(&store, Tool::YtDlp).error.as_deref(),
        Some("Couldn't reach github.com to get yt-dlp; will retry.")
    );

    // Without a copy, the same failure is an error a task will retry.
    let empty = World::new().await;
    empty.server.set_route(LATEST_PATH, Route::status(502));
    let err = ensure_now(&empty.store(), Tool::YtDlp).await.unwrap_err();
    assert!(err.is_transient());
    let as_engine_error: rustdm::error::Error = err.into();
    assert!(as_engine_error.is_transient());
}

#[tokio::test]
async fn a_deleted_program_is_reinstalled() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let installed = ensure_now(&world.store(), Tool::YtDlp).await.unwrap();
    std::fs::remove_file(&installed.exe).unwrap();

    let again = world
        .store()
        .ensure(Tool::YtDlp, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();

    assert_eq!(again.version, V1);
    assert!(again.exe.is_file());
}

#[tokio::test]
async fn a_replaced_program_is_noticed_on_first_use_in_a_run() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let installed = ensure_now(&world.store(), Tool::YtDlp).await.unwrap();
    std::fs::write(&installed.exe, fake_program("replaced")).unwrap();

    // A new store is a new daemon run.
    let store = world.store();
    let again = store
        .ensure(Tool::YtDlp, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();

    assert_eq!(std::fs::read(&again.exe).unwrap(), fake_program(V1));
}

#[tokio::test]
async fn a_corrupt_tools_file_is_moved_aside_and_nothing_counts_as_installed() {
    let world = World::new().await;
    std::fs::create_dir_all(world.root()).unwrap();
    std::fs::write(world.root().join("tools.json"), b"{broken").unwrap();

    let store = world.store();

    assert!(world.root().join("tools.json.corrupt").exists());
    assert!(store.status().iter().all(|i| i.installed.is_none()));
}

#[tokio::test]
async fn startup_removes_folders_nothing_refers_to() {
    let world = World::new().await;
    let root = world.root();
    std::fs::create_dir_all(root.join("yt-dlp/2026.01.01")).unwrap();
    std::fs::create_dir_all(root.join(".staging/ffmpeg-1.0")).unwrap();
    std::fs::create_dir_all(root.join(".staging/leftover")).unwrap();

    let _store = world.store();

    assert!(!root.join("yt-dlp/2026.01.01").exists());
    assert!(!root.join(".staging/ffmpeg-1.0").exists());
    assert!(!root.join(".staging/leftover").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_program_that_will_not_start_is_not_installed() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    ensure_now(&store, Tool::YtDlp).await.unwrap();

    // The new version exits 3: the old one stays in use.
    publish_ytdlp(&world.server, V2, &failing_program(3));
    let kept = ensure_now(&store, Tool::YtDlp).await.unwrap();
    assert_eq!(kept.version, V1);
    assert_eq!(
        info(&store, Tool::YtDlp).error,
        Some(format!(
            "yt-dlp {V2} didn't start (exit 3); still using {V1}."
        ))
    );
    assert_eq!(version_folders(&world.root(), "yt-dlp"), vec![V1]);

    // With nothing installed, it is an error.
    let empty = World::new().await;
    publish_ytdlp(&empty.server, V2, &failing_program(3));
    let err = ensure_now(&empty.store(), Tool::YtDlp).await.unwrap_err();
    assert!(matches!(err, ToolError::RunCheckFailed { .. }), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_program_that_vanishes_after_starting_is_reported_as_quarantined() {
    let world = World::new().await;
    let vanishing = format!("#!/bin/sh\necho {V1}\nrm -- \"$0\"\n").into_bytes();
    publish_ytdlp(&world.server, V1, &vanishing);

    let err = ensure_now(&world.store(), Tool::YtDlp).await.unwrap_err();

    assert!(matches!(err, ToolError::Quarantined { .. }), "{err}");
    assert_eq!(
        err.to_string()
            .contains("Windows Security may have quarantined it"),
        cfg!(windows)
    );
    assert!(version_folders(&world.root(), "yt-dlp").is_empty());
}

// --- ffmpeg and deno, from pinned zip files ---

fn store_with_pin(world: &World, tool: Tool, pin: Pin) -> Arc<ToolStore> {
    let mut sources = test_sources(&world.server);
    sources.pins.insert(tool, pin);
    Arc::new(ToolStore::with_sources(world.root(), build_client(), sources).unwrap())
}

#[cfg(unix)]
#[tokio::test]
async fn ffmpeg_is_unpacked_from_its_pinned_archive() {
    let world = World::new().await;
    let archive = zip_bytes(&[
        ("ffmpeg-8.1/bin/ffmpeg", fake_program("8.1.3")),
        ("ffmpeg-8.1/bin/helper.dll", b"dll".to_vec()),
        ("ffmpeg-8.1/bin/ffplay", b"unwanted".to_vec()),
        ("ffmpeg-8.1/include/x.h", b"unwanted".to_vec()),
    ]);
    world
        .server
        .set_route("/pins/tool.zip", Route::ok(archive.clone()));
    let pin = zip_pin(
        &archive,
        "8.1.3",
        1,
        &["bin/ffmpeg", "bin/*.dll"],
        "bin/ffmpeg",
        "bin",
    );
    let store = store_with_pin(&world, Tool::Ffmpeg, pin);

    let installed = store
        .ensure(Tool::Ffmpeg, Freshness::CheckIfStale, FetchCtx::new())
        .await
        .unwrap();

    assert_eq!(installed.version, "8.1.3");
    assert_eq!(installed.dir, world.root().join("ffmpeg/8.1.3/bin"));
    assert!(installed.exe.is_file());
    assert!(installed.dir.join("helper.dll").is_file());
    assert!(!installed.dir.join("ffplay").exists());
    assert!(!world.root().join("ffmpeg/8.1.3/include").exists());

    // Installed already: no more network use, and no GitHub lookup for pinned tools.
    let calls = world.server.requests().len();
    store
        .ensure(Tool::Ffmpeg, Freshness::CheckNow, FetchCtx::new())
        .await
        .unwrap();
    assert_eq!(world.server.requests().len(), calls);
    assert_eq!(world.api_calls(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn a_new_pin_installs_on_next_use_and_keeps_the_old_one() {
    let world = World::new().await;
    let v1 = zip_bytes(&[("deno", fake_program("2.9.7"))]);
    world
        .server
        .set_route("/pins/tool.zip", Route::ok(v1.clone()));
    let store = store_with_pin(
        &world,
        Tool::Deno,
        zip_pin(&v1, "2.9.7", 0, &["deno"], "deno", ""),
    );
    store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();

    // A newer rustdm ships a newer pin.
    let v2 = zip_bytes(&[("deno", fake_program("2.9.8"))]);
    world
        .server
        .set_route("/pins/tool.zip", Route::ok(v2.clone()));
    let store = store_with_pin(
        &world,
        Tool::Deno,
        zip_pin(&v2, "2.9.8", 0, &["deno"], "deno", ""),
    );
    let installed = store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();

    assert_eq!(installed.version, "2.9.8");
    assert_eq!(info(&store, Tool::Deno).previous.as_deref(), Some("2.9.7"));
    assert_eq!(
        version_folders(&world.root(), "deno"),
        vec!["2.9.7", "2.9.8"]
    );
}

#[tokio::test]
async fn an_archive_with_an_entry_outside_its_folder_installs_nothing() {
    let world = World::new().await;
    let archive = zip_bytes(&[
        ("deno", fake_program("2.9.8")),
        ("../escaped.txt", b"evil".to_vec()),
    ]);
    world
        .server
        .set_route("/pins/tool.zip", Route::ok(archive.clone()));
    let store = store_with_pin(
        &world,
        Tool::Deno,
        zip_pin(&archive, "2.9.8", 0, &["deno"], "deno", ""),
    );

    let err = store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap_err();

    assert!(err.to_string().contains("unsafe name"), "{err}");
    assert!(version_folders(&world.root(), "deno").is_empty());
    assert!(!world.dir.path().join("escaped.txt").exists());
    assert!(!world.root().join("escaped.txt").exists());
}

#[tokio::test]
async fn a_pinned_archive_with_the_wrong_hash_is_refused() {
    let world = World::new().await;
    let archive = zip_bytes(&[("deno", fake_program("2.9.8"))]);
    world
        .server
        .set_route("/pins/tool.zip", Route::ok(archive.clone()));
    let mut pin = zip_pin(&archive, "2.9.8", 0, &["deno"], "deno", "");
    pin.sha256 = "0".repeat(64);
    let store = store_with_pin(&world, Tool::Deno, pin);

    let err = store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::ChecksumMismatch { .. }), "{err}");
    assert!(version_folders(&world.root(), "deno").is_empty());
}

#[tokio::test]
async fn a_tool_with_no_build_for_this_system_says_so() {
    let world = World::new().await;
    let err = world
        .store()
        .ensure(Tool::Ffmpeg, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "ffmpeg isn't available for this system");
}

// --- a saved download that cannot be continued ---

fn staging_of(world: &World, version: &str) -> PathBuf {
    world
        .root()
        .join(".staging")
        .join(format!("yt-dlp-{version}"))
}

#[tokio::test]
async fn an_unreadable_saved_download_is_started_over() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    let staging = staging_of(&world, V1);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join(format!("{TEST_ASSET}.part.rdm")), b"{not json").unwrap();
    std::fs::write(staging.join(format!("{TEST_ASSET}.part")), b"junk").unwrap();

    let installed = ensure_now(&store, Tool::YtDlp).await.unwrap();

    assert_eq!(installed.version, V1);
    assert_eq!(std::fs::read(&installed.exe).unwrap(), fake_program(V1));
    assert_eq!(info(&store, Tool::YtDlp).error, None);
}

#[tokio::test]
async fn a_saved_download_of_a_file_that_changed_is_started_over() {
    let world = World::new().await;
    publish_ytdlp(&world.server, V1, &fake_program(V1));
    let store = world.store();
    let staging = staging_of(&world, V1);
    std::fs::create_dir_all(&staging).unwrap();
    let output = staging.join(TEST_ASSET);
    let url = format!(
        "{}{TEST_REPO}/releases/download/{V1}/{TEST_ASSET}",
        world.server.url()
    );
    // Saved from a file of another size: resuming it reports "file changed".
    let state = rustdm::state::State {
        version: rustdm::state::STATE_VERSION,
        url: url.clone(),
        final_url: url,
        total: 4242,
        validator: Default::default(),
        completed: vec![],
        output: output.clone(),
        connections: Some(4),
    };
    rustdm::state::save_atomic(&rustdm::state::state_path(&output), &state).unwrap();
    std::fs::write(rustdm::state::part_path(&output), vec![0u8; 4242]).unwrap();

    let installed = ensure_now(&store, Tool::YtDlp).await.unwrap();

    assert_eq!(installed.version, V1);
    assert_eq!(std::fs::read(&installed.exe).unwrap(), fake_program(V1));
}

// --- links that expire part-way ---

#[tokio::test]
async fn a_link_that_keeps_expiring_stops_after_a_few_tries() {
    let world = World::new().await;
    let archive = zip_bytes(&[("deno", fake_program("2.9.8"))]);
    // The probe is answered, then every data request is refused with 403.
    let expiring = TestServer::start(archive.clone(), Mode::ExpireAfter(1)).await;
    let mut sources = test_sources(&world.server);
    sources.download_base = expiring.url().as_str().trim_end_matches('/').to_string();
    sources.pins.insert(
        Tool::Deno,
        zip_pin(&archive, "2.9.8", 0, &["deno"], "deno", ""),
    );
    let store = ToolStore::with_sources(world.root(), build_client(), sources).unwrap();

    let err = store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap_err();

    let text = err.to_string();
    assert!(
        text.contains("kept expiring") || text.contains("refused the deno download"),
        "{text}"
    );
    assert!(version_folders(&world.root(), "deno").is_empty());
    // It gave up after a handful of tries, not for ever.
    assert!(
        expiring.requests().len() < 40,
        "{}",
        expiring.requests().len()
    );
}

// --- rolling back a pinned tool ---

#[cfg(unix)]
#[tokio::test]
async fn rolling_back_a_pinned_tool_sticks_until_the_user_updates() {
    let world = World::new().await;
    let pin_of = |version: &str| {
        let archive = zip_bytes(&[("deno", fake_program(version))]);
        let pin = zip_pin(&archive, version, 0, &["deno"], "deno", "");
        (archive, pin)
    };
    let (v1, pin1) = pin_of("2.9.7");
    world.server.set_route("/pins/tool.zip", Route::ok(v1));
    store_with_pin(&world, Tool::Deno, pin1)
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();
    let (v2, pin2) = pin_of("2.9.8");
    world.server.set_route("/pins/tool.zip", Route::ok(v2));
    let store = store_with_pin(&world, Tool::Deno, pin2.clone());
    store
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await
        .unwrap();

    let rolled = store.rollback(Tool::Deno).unwrap();
    assert_eq!(rolled.installed.as_deref(), Some("2.9.7"));

    // Neither this run nor the next downloads the rolled-back version again.
    let calls = world.server.requests().len();
    for fresh in [Freshness::AsInstalled, Freshness::CheckNow] {
        let kept = store.ensure(Tool::Deno, fresh, FetchCtx::new()).await;
        assert_eq!(kept.unwrap().version, "2.9.7");
    }
    let next_run = store_with_pin(&world, Tool::Deno, pin2);
    let kept = next_run
        .ensure(Tool::Deno, Freshness::AsInstalled, FetchCtx::new())
        .await;
    assert_eq!(kept.unwrap().version, "2.9.7");
    assert_eq!(world.server.requests().len(), calls, "no new request");
    assert_eq!(
        version_folders(&world.root(), "deno"),
        vec!["2.9.7", "2.9.8"]
    );
    assert_eq!(
        info(&next_run, Tool::Deno).previous.as_deref(),
        Some("2.9.8")
    );

    // Asking for the update is a choice to go forward again; the folder that
    // was listed as previous is replaced only once the new one is in place.
    next_run.start_update(Some(Tool::Deno));
    wait_until_idle(&next_run, Tool::Deno).await;
    let shown = info(&next_run, Tool::Deno);
    assert_eq!(shown.installed.as_deref(), Some("2.9.8"), "{shown:?}");
    assert_eq!(shown.previous.as_deref(), Some("2.9.7"));
    assert_eq!(
        version_folders(&world.root(), "deno"),
        vec!["2.9.7", "2.9.8"]
    );
    assert!(!world
        .root()
        .join(".staging")
        .join("replaced-deno-2.9.8")
        .exists());
}
