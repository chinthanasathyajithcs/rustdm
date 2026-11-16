//! A download's cookies, Referer and User-Agent through the daemon: kept in the
//! secret store while the download needs them, gone when it does not.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::secrets::{SecretError, SecretStore};
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, DownloadStatus, DownloadSummary, CODE_INVALID_PARAMS,
};
use rustdm::request_context::{ContextCookie, RequestContext};
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::path::Path;
use std::time::{Duration, Instant};

const REFERER: &str = "https://files.example.com/page";
const USER_AGENT: &str = "TestBrowser/1.0";
const BODY_BYTES: u64 = 2 * 1024 * 1024;
/// Per connection; slow enough that a download can be stopped part way.
const SLOW: u64 = 256 * 1024;

fn cookie(value: &str) -> ContextCookie {
    ContextCookie {
        name: "sid".to_string(),
        value: value.to_string(),
        domain: "127.0.0.1".to_string(),
        path: "/".to_string(),
        secure: false,
        host_only: true,
        expires: None,
    }
}

fn context(value: &str) -> RequestContext {
    RequestContext {
        cookies: vec![cookie(value)],
        referer: Some(REFERER.to_string()),
        user_agent: Some(USER_AGENT.to_string()),
    }
}

async fn slow_server() -> TestServer {
    TestServer::start(
        pattern_bytes(BODY_BYTES),
        Mode::Throttle {
            bytes_per_sec: SLOW,
        },
    )
    .await
}

fn add_params(
    server: &TestServer,
    folder: &Path,
    context: Option<RequestContext>,
) -> AddDownloadParams {
    AddDownloadParams {
        url: server.url().join("file.bin").unwrap().to_string(),
        destination: Some(folder.to_path_buf()),
        filename: Some("file.bin".to_string()),
        connections: Some(2),
        max_bytes_per_sec: None,
        context,
    }
}

fn secret_file(data: &Path, id: &str) -> std::path::PathBuf {
    data.join("secrets").join(format!("{id}.cookies"))
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

/// Stop the core the way a daemon shutdown does, and wait until it is quiet.
async fn stop(core: &DaemonCore) {
    core.shutdown();
    assert!(core.wait_until_settled(Duration::from_secs(15)).await);
}

fn open_core(data: &Path) -> DaemonCore {
    DaemonCore::with_store(QueueStore::in_dir(data)).unwrap()
}

#[tokio::test]
async fn a_download_with_context_sends_it_everywhere_and_its_secret_is_gone_when_it_completes() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    server.require_header("cookie", "sid=hush-hush");
    server.require_header("referer", REFERER);
    server.require_header("user-agent", USER_AGENT);
    let core = open_core(data.path());

    let added = core
        .add_download(add_params(
            &server,
            folder.path(),
            Some(context("hush-hush")),
        ))
        .unwrap();
    assert!(secret_file(data.path(), &added.id).exists());
    let queue = std::fs::read_to_string(data.path().join("queue.json")).unwrap();
    assert!(
        !queue.contains("hush-hush"),
        "the queue file holds no cookie value"
    );
    assert!(queue.contains(REFERER));

    wait_for(&core, &added.id, DownloadStatus::Completed).await;

    assert_eq!(
        std::fs::read(folder.path().join("file.bin")).unwrap(),
        pattern_bytes(BODY_BYTES)
    );
    assert!(!secret_file(data.path(), &added.id).exists());
    let queue = std::fs::read_to_string(data.path().join("queue.json")).unwrap();
    assert!(queue.contains("\"has_cookies\": false"));
}

#[tokio::test]
async fn cancelling_a_download_deletes_its_secret() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    assert!(secret_file(data.path(), &added.id).exists());

    core.cancel_download(CancelDownloadParams {
        id: added.id.clone(),
        delete_partial: true,
    })
    .unwrap();

    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn removing_a_paused_download_deletes_its_secret_but_pausing_keeps_it() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();

    core.pause_download(&added.id).unwrap();
    wait_for(&core, &added.id, DownloadStatus::Paused).await;
    assert!(
        secret_file(data.path(), &added.id).exists(),
        "a paused download may resume"
    );

    core.remove_download(&added.id).unwrap();
    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_403_at_the_first_request_deletes_the_secret_and_says_to_sign_in_again() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = TestServer::start(pattern_bytes(1024), Mode::Status(403)).await;
    let core = open_core(data.path());

    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    let failed = wait_for(&core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some(
            "sign-in expired (HTTP 403); start the download again from the browser to continue it"
        )
    );
    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_403_part_way_pauses_the_download_and_deletes_the_secret() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    // The probe and one connection are answered; the second connection gets 403.
    let server = TestServer::start(pattern_bytes(BODY_BYTES), Mode::ExpireAfter(2)).await;
    let core = open_core(data.path());

    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    let paused = wait_for(&core, &added.id, DownloadStatus::Paused).await;

    assert!(paused
        .error_message
        .as_deref()
        .unwrap()
        .starts_with("sign-in expired (HTTP 403)"));
    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_failed_download_keeps_its_secret_so_it_can_be_resumed() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    // A server that never gives the file: 404 is a failure that is not a sign-in problem.
    let server = TestServer::start(pattern_bytes(1024), Mode::Status(404)).await;
    let core = open_core(data.path());

    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    let failed = wait_for(&core, &added.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some("link expired or removed (HTTP 404)")
    );
    assert!(secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_restart_brings_the_cookies_back_and_the_download_completes() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    server.require_header("cookie", "sid=hush-hush");
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(
            &server,
            folder.path(),
            Some(context("hush-hush")),
        ))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    stop(&first).await;
    drop(first);
    assert!(secret_file(data.path(), &added.id).exists());

    let second = open_core(data.path());
    second.schedule();
    wait_for(&second, &added.id, DownloadStatus::Completed).await;

    assert_eq!(
        std::fs::read(folder.path().join("file.bin")).unwrap(),
        pattern_bytes(BODY_BYTES)
    );
    assert!(!secret_file(data.path(), &added.id).exists());
    // Every request, before and after the restart, carried the cookie.
    assert!(server
        .requests()
        .iter()
        .all(|r| r.cookie.as_deref() == Some("sid=hush-hush")));
}

#[tokio::test]
async fn an_unreadable_secret_pauses_the_download_with_a_message_instead_of_starting_it() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    stop(&first).await;
    drop(first);
    std::fs::write(secret_file(data.path(), &added.id), b"scribbles").unwrap();
    let requests_before = server.requests().len();

    let second = open_core(data.path());
    second.schedule();

    let summary = second.get_status(&added.id).unwrap();
    assert_eq!(summary.status, DownloadStatus::Paused);
    assert_eq!(
        summary.error_message.as_deref(),
        Some("saved sign-in could not be read; start the download again from the browser to continue it")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.requests().len(),
        requests_before,
        "nothing was started"
    );
    // The damaged file is swept, and the pause survives another restart.
    assert!(!secret_file(data.path(), &added.id).exists());
    drop(second);
    let third = open_core(data.path());
    assert_eq!(
        third.get_status(&added.id).unwrap().status,
        DownloadStatus::Paused
    );
}

#[tokio::test]
async fn a_missing_secret_pauses_the_download_the_same_way() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    stop(&first).await;
    drop(first);
    std::fs::remove_file(secret_file(data.path(), &added.id)).unwrap();

    let second = open_core(data.path());

    assert_eq!(
        second.get_status(&added.id).unwrap().status,
        DownloadStatus::Paused
    );
}

#[tokio::test]
async fn starting_up_sweeps_secrets_with_no_download_and_leftover_temp_files() {
    let data = tempfile::tempdir().unwrap();
    let store = SecretStore::in_dir(data.path());
    store.save("dl_41", &[cookie("orphan")]).unwrap();
    let leftover = data.path().join("secrets").join("dl_42.cookies.tmp");
    std::fs::write(&leftover, b"half written").unwrap();

    let _core = open_core(data.path());

    assert!(matches!(store.load("dl_41"), Err(SecretError::Missing)));
    assert!(!leftover.exists());
}

#[tokio::test]
async fn adding_the_same_link_again_from_the_browser_continues_a_paused_download() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(
            &server,
            folder.path(),
            Some(context("old-cookie")),
        ))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    core.pause_download(&added.id).unwrap();
    wait_for(&core, &added.id, DownloadStatus::Paused).await;
    let kept = std::fs::metadata(folder.path().join("file.bin.part"))
        .unwrap()
        .len();
    assert!(kept > 0);

    server.require_header("cookie", "sid=new-cookie");
    let again = core
        .add_download(add_params(
            &server,
            folder.path(),
            Some(context("new-cookie")),
        ))
        .unwrap();

    assert_eq!(again.id, added.id, "the same download carries on");
    assert_eq!(core.list_downloads().len(), 1);
    let store = SecretStore::in_dir(data.path());
    assert_eq!(store.load(&added.id).unwrap()[0].value, "new-cookie");
    wait_for(&core, &added.id, DownloadStatus::Completed).await;
    assert_eq!(
        std::fs::read(folder.path().join("file.bin")).unwrap(),
        pattern_bytes(BODY_BYTES)
    );
    let resumed = server
        .requests()
        .into_iter()
        .filter(|r| r.cookie.as_deref() == Some("sid=new-cookie") && r.range.is_some())
        .filter_map(|r| r.range)
        .any(|range| range != "bytes=0-0" && !range.starts_with("bytes=0-"));
    assert!(resumed, "it continued from the partial file");
}

#[tokio::test]
async fn adding_the_same_link_again_continues_a_download_that_the_site_turned_down() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = TestServer::start(pattern_bytes(BODY_BYTES), Mode::ExpireAfter(2)).await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("stale"))))
        .unwrap();
    wait_for(&core, &added.id, DownloadStatus::Paused).await;
    assert!(!secret_file(data.path(), &added.id).exists());

    let again = core
        .add_download(add_params(&server, folder.path(), Some(context("fresh"))))
        .unwrap();

    assert_eq!(again.id, added.id);
    assert_eq!(core.list_downloads().len(), 1);
    assert_eq!(again.error_message, None);
    assert_eq!(
        SecretStore::in_dir(data.path()).load(&added.id).unwrap()[0].value,
        "fresh"
    );
}

/// A paused download with a saved sign-in, and the core it lives in.
async fn paused_with_cookies(
    data: &Path,
    folder: &Path,
    server: &TestServer,
) -> (DaemonCore, DownloadSummary) {
    let core = open_core(data);
    let added = core
        .add_download(add_params(server, folder, Some(context("keep-me"))))
        .unwrap();
    core.pause_download(&added.id).unwrap();
    wait_for(&core, &added.id, DownloadStatus::Paused).await;
    (core, added)
}

/// The paused download was left alone and `other` is a separate, new download.
fn assert_left_alone(
    core: &DaemonCore,
    data: &Path,
    paused: &DownloadSummary,
    other: &DownloadSummary,
) {
    assert_ne!(other.id, paused.id, "a new download, not a takeover");
    assert!(core.list_downloads().len() >= 2);
    assert_eq!(
        core.get_status(&paused.id).unwrap().status,
        DownloadStatus::Paused
    );
    let kept = SecretStore::in_dir(data).load(&paused.id).unwrap();
    assert_eq!(
        kept[0].value, "keep-me",
        "the paused download keeps its sign-in"
    );
    core.cancel_download(CancelDownloadParams {
        id: other.id.clone(),
        delete_partial: true,
    })
    .unwrap();
}

#[tokio::test]
async fn adding_with_only_a_referer_starts_a_new_download_and_leaves_the_paused_one_alone() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
    let referer_only = RequestContext {
        referer: Some(REFERER.to_string()),
        ..Default::default()
    };

    let other = core
        .add_download(add_params(&server, folder.path(), Some(referer_only)))
        .unwrap();

    assert_left_alone(&core, data.path(), &paused, &other);
}

#[tokio::test]
async fn adding_with_only_expired_cookies_does_not_wipe_the_paused_downloads_secret() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
    let mut stale = context("stale");
    stale.cookies[0].expires = Some(1);

    let other = core
        .add_download(add_params(&server, folder.path(), Some(stale)))
        .unwrap();

    assert_left_alone(&core, data.path(), &paused, &other);
}

#[tokio::test]
async fn adding_with_another_file_name_or_folder_starts_a_new_download() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;

    let renamed = AddDownloadParams {
        filename: Some("other.bin".to_string()),
        ..add_params(&server, folder.path(), Some(context("fresh")))
    };
    let other = core.add_download(renamed).unwrap();
    assert_ne!(other.id, paused.id);
    core.cancel_download(CancelDownloadParams {
        id: other.id,
        delete_partial: true,
    })
    .unwrap();

    let moved = add_params(&server, elsewhere.path(), Some(context("fresh")));
    let other = core.add_download(moved).unwrap();
    assert_left_alone(&core, data.path(), &paused, &other);
}

#[tokio::test]
async fn a_browser_add_without_a_folder_or_name_still_continues_the_paused_download() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
    let bare = AddDownloadParams {
        destination: None,
        filename: None,
        ..add_params(&server, folder.path(), Some(context("fresh")))
    };

    let again = core.add_download(bare).unwrap();

    assert_eq!(again.id, paused.id);
    core.cancel_download(CancelDownloadParams {
        id: again.id,
        delete_partial: true,
    })
    .unwrap();
}

#[tokio::test]
async fn adding_a_running_download_again_is_still_a_duplicate() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();

    let err = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap_err();

    assert_eq!(err.code, rustdm::ipc::protocol::CODE_DUPLICATE_DOWNLOAD);
    assert_eq!(core.list_downloads().len(), 1);
    core.cancel_download(CancelDownloadParams {
        id: added.id,
        delete_partial: true,
    })
    .unwrap();
}

#[tokio::test]
async fn a_context_with_a_bad_field_is_refused_by_name_and_creates_nothing() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let mut bad = context("secret-value\r\nX-Evil: 1");
    bad.referer = None;

    let err = core
        .add_download(add_params(&server, folder.path(), Some(bad)))
        .unwrap_err();

    assert_eq!(err.code, CODE_INVALID_PARAMS);
    assert!(err.message.contains("cookies[0].value"), "{}", err.message);
    assert!(!err.message.contains("secret-value"));
    assert!(core.list_downloads().is_empty());
}

#[tokio::test]
async fn cookies_that_have_already_expired_are_dropped_and_nothing_is_saved() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let mut stale = context("abc");
    stale.cookies[0].expires = Some(1);

    let added = core
        .add_download(add_params(&server, folder.path(), Some(stale)))
        .unwrap();

    assert!(!secret_file(data.path(), &added.id).exists());
    let queue = std::fs::read_to_string(data.path().join("queue.json")).unwrap();
    assert!(queue.contains("\"has_cookies\": false"));
    core.cancel_download(CancelDownloadParams {
        id: added.id,
        delete_partial: true,
    })
    .unwrap();
}

#[tokio::test]
async fn a_download_without_context_never_touches_the_secret_store() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());

    let added = core
        .add_download(add_params(&server, folder.path(), None))
        .unwrap();

    assert!(!secret_file(data.path(), &added.id).exists());
    assert!(!data.path().join("secrets").exists());
    core.cancel_download(CancelDownloadParams {
        id: added.id,
        delete_partial: true,
    })
    .unwrap();
}
