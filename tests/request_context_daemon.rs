//! A download's cookies, Referer and User-Agent through the daemon: kept in the
//! secret store while the download needs them, gone when it does not.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::secrets::{SecretError, SecretStore};
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{
    AddDownloadParams, AddedBy, CancelDownloadParams, DownloadStatus, DownloadSummary,
    CODE_INVALID_PARAMS,
};
use rustdm::request_context::{ContextCookie, RequestContext};
use rustdm::testing::video::{write_script, ToolResponse, ToolScript, VideoWorld};
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
        title: None,
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
        quality: None,
        added_by: None,
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
async fn cookies_that_ran_out_while_the_daemon_was_off_pause_the_download() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    stop(&first).await;
    drop(first);
    let lapsed = ContextCookie {
        expires: Some(1),
        ..cookie("abc")
    };
    SecretStore::in_dir(data.path())
        .save(&added.id, &[lapsed])
        .unwrap();
    let requests_before = server.requests().len();

    let second = open_core(data.path());
    second.schedule();

    let summary = second.get_status(&added.id).unwrap();
    assert_eq!(summary.status, DownloadStatus::Paused);
    assert_eq!(
        summary.error_message.as_deref(),
        Some("sign-in expired (the saved cookies have run out); start the download again from the browser to continue it")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.requests().len(),
        requests_before,
        "nothing was started"
    );
    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_secret_that_cannot_be_read_keeps_its_file_and_pauses_the_download() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    stop(&first).await;
    drop(first);
    // A folder where the file should be: reading it fails, but not as "missing".
    let path = secret_file(data.path(), &added.id);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    let second = open_core(data.path());

    let summary = second.get_status(&added.id).unwrap();
    assert_eq!(summary.status, DownloadStatus::Paused);
    assert!(summary
        .error_message
        .as_deref()
        .is_some_and(|m| m.starts_with("saved sign-in could not be read")));
    assert!(path.exists(), "the file is kept in case the trouble passes");
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

#[tokio::test]
async fn adding_the_same_link_again_continues_a_download_that_is_waiting_to_retry() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    // A server that is having trouble: the download waits to retry.
    let server = TestServer::start(pattern_bytes(1024), Mode::Status(503)).await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("old"))))
        .unwrap();
    wait_for(&core, &added.id, DownloadStatus::Retrying).await;

    let again = core
        .add_download(add_params(&server, folder.path(), Some(context("new"))))
        .unwrap();

    assert_eq!(again.id, added.id, "the waiting download carries on");
    assert_eq!(core.list_downloads().len(), 1);
    let store = SecretStore::in_dir(data.path());
    assert_eq!(store.load(&added.id).unwrap()[0].value, "new");
}

#[tokio::test]
async fn adding_the_same_link_with_no_cookies_does_not_take_over_a_download_waiting_to_retry() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = TestServer::start(pattern_bytes(1024), Mode::Status(503)).await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("keep-me"))))
        .unwrap();
    wait_for(&core, &added.id, DownloadStatus::Retrying).await;

    let other = core
        .add_download(add_params(&server, folder.path(), None))
        .unwrap();

    assert_ne!(other.id, added.id);
    let store = SecretStore::in_dir(data.path());
    assert_eq!(store.load(&added.id).unwrap()[0].value, "keep-me");
}

#[tokio::test]
async fn cancelling_leaves_no_cookies_behind_even_after_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();

    first
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: false,
        })
        .unwrap();
    assert!(first.wait_until_settled(Duration::from_secs(15)).await);

    assert!(!secret_file(data.path(), &added.id).exists());
    let queue = std::fs::read_to_string(data.path().join("queue.json")).unwrap();
    assert!(queue.contains("\"has_cookies\": false"));
    drop(first);
    let second = open_core(data.path());
    assert_eq!(
        second.get_status(&added.id).unwrap().status,
        DownloadStatus::Cancelled
    );
    assert!(!secret_file(data.path(), &added.id).exists());
}

#[tokio::test]
async fn a_cancel_racing_a_continue_never_leaves_a_secret_for_a_cancelled_download() {
    let server = slow_server().await;
    for round in 0..10 {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
        let core = std::sync::Arc::new(core);
        let params = add_params(&server, folder.path(), Some(context("fresh")));

        let adder = {
            let core = std::sync::Arc::clone(&core);
            tokio::task::spawn_blocking(move || core.add_download(params))
        };
        let canceller = {
            let core = std::sync::Arc::clone(&core);
            let id = paused.id.clone();
            tokio::task::spawn_blocking(move || {
                core.cancel_download(CancelDownloadParams {
                    id,
                    delete_partial: false,
                })
            })
        };
        let added = adder.await.unwrap().unwrap();
        canceller.await.unwrap().unwrap();

        // Whichever came first, the secret on disk matches the download's state.
        let status = core.get_status(&paused.id).unwrap().status;
        let has_file = secret_file(data.path(), &paused.id).exists();
        assert_eq!(
            has_file,
            status != DownloadStatus::Cancelled,
            "round {round}"
        );
        if added.id != paused.id {
            // The cancel came first; the add made a download of its own, with its own secret.
            assert!(
                secret_file(data.path(), &added.id).exists(),
                "round {round}"
            );
        }
        stop(&core).await;
    }
}

/// Wait until the server's address can be used again after `old` was dropped.
async fn restart_server_at(old: TestServer, body: Vec<u8>, mode: Mode) -> TestServer {
    let address = format!(
        "127.0.0.1:{}",
        old.url().port().expect("the test server has a port")
    );
    drop(old);
    for _ in 0..100 {
        if let Ok(server) = TestServer::start_at(&address, body.clone(), mode.clone()).await {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("could not bring the server back at {address}");
}

#[tokio::test]
async fn a_sign_in_page_on_resume_fails_the_download_and_deletes_the_secret() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
    // A web page is looked up for a video before it fails; the stand-in yt-dlp
    // finds none, so nothing reaches the real tools host.
    let world = VideoWorld::new().await;
    write_script(
        &world.ytdlp.exe,
        &ToolScript {
            responses: vec![ToolResponse {
                stderr: "ERROR: Unsupported URL: http://127.0.0.1/file.bin".to_string(),
                exit_code: 1,
                ..ToolResponse::default()
            }],
        },
    );
    let core = core.with_tools(world.store.clone());
    // The site now answers the link with a sign-in page.
    let _page = restart_server_at(
        server,
        pattern_bytes(BODY_BYTES),
        Mode::ContentType("text/html".to_string()),
    )
    .await;

    core.resume_download(&paused.id).unwrap();
    let failed = wait_for(&core, &paused.id, DownloadStatus::Failed).await;

    assert_eq!(
        failed.error_message.as_deref(),
        Some("the link opened a web page, not a file; if the site wants you to sign in, sign in and start the download again from the browser")
    );
    assert!(!secret_file(data.path(), &paused.id).exists());
}

#[tokio::test]
async fn after_the_cookies_are_dropped_a_resume_still_sends_the_referer_and_user_agent() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = TestServer::start(pattern_bytes(BODY_BYTES), Mode::ExpireAfter(2)).await;
    let core = open_core(data.path());
    let added = core
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    wait_for(&core, &added.id, DownloadStatus::Paused).await;
    assert!(!secret_file(data.path(), &added.id).exists());
    let before = server.requests().len();

    core.resume_download(&added.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while server.requests().len() == before {
        assert!(Instant::now() < deadline, "the resume sent nothing");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let later = &server.requests()[before..];
    assert!(later.iter().all(|r| r.cookie.is_none()), "{later:?}");
    assert!(later
        .iter()
        .all(|r| r.referer.as_deref() == Some(REFERER)
            && r.user_agent.as_deref() == Some(USER_AGENT)));
}

#[tokio::test]
async fn after_a_restart_a_cancelled_download_that_is_resumed_keeps_its_referer_and_user_agent() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let first = open_core(data.path());
    let added = first
        .add_download(add_params(&server, folder.path(), Some(context("abc"))))
        .unwrap();
    first
        .cancel_download(CancelDownloadParams {
            id: added.id.clone(),
            delete_partial: false,
        })
        .unwrap();
    assert!(first.wait_until_settled(Duration::from_secs(15)).await);
    drop(first);
    let before = server.requests().len();

    let second = open_core(data.path());
    second.resume_download(&added.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while server.requests().len() == before {
        assert!(Instant::now() < deadline, "the resume sent nothing");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let later = &server.requests()[before..];
    assert!(later
        .iter()
        .all(|r| r.referer.as_deref() == Some(REFERER)
            && r.user_agent.as_deref() == Some(USER_AGENT)));
    assert!(later.iter().all(|r| r.cookie.is_none()));
    stop(&second).await;
}

#[tokio::test]
async fn a_cookie_the_server_sets_stays_with_the_download_through_a_pause() {
    use rustdm::daemon::TaskRecord;
    use rustdm::engine::{Outcome, PauseReason};
    let server = TestServer::start(pattern_bytes(64 * 1024), Mode::SetCookieThenRedirect).await;
    let ctx = RequestContext {
        referer: Some(REFERER.to_string()),
        ..Default::default()
    };
    let client =
        rustdm::request_context::client_for(&ctx, rustdm::engine::Timeouts::default()).unwrap();
    let mut record = TaskRecord::new(
        "dl_1".to_string(),
        server.url().join("file.bin").unwrap().to_string(),
        std::path::PathBuf::from("/downloads"),
        None,
        1,
    );
    record.set_context(&ctx, Some(client));
    // The first request is redirected, and the site sets its cookie on the way.
    let first = record.client.clone().unwrap();
    first.get(&record.url).send().await.unwrap();

    record.status = DownloadStatus::Active;
    record.running = true;
    record.apply_outcome(Ok(Outcome::Paused {
        reason: PauseReason::Cancelled,
        state: std::path::PathBuf::from("/downloads/file.bin.part.rdm"),
    }));
    assert_eq!(record.status, DownloadStatus::Paused);

    let resumed = record.client.clone().expect("the client is kept");
    resumed
        .get(server.url().join("moved/file.bin").unwrap())
        .send()
        .await
        .unwrap();
    let last = server.requests().pop().unwrap();
    assert_eq!(last.path, "/moved/file.bin");
    assert_eq!(last.cookie.as_deref(), Some("sid=redirect-cookie"));
}

#[tokio::test]
async fn an_add_marked_browser_is_listed_as_browser() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let core = open_core(data.path());
    let marked = AddDownloadParams {
        added_by: Some(AddedBy::Browser),
        ..add_params(&server, folder.path(), None)
    };

    let added = core.add_download(marked).unwrap();

    assert_eq!(added.added_by, Some(AddedBy::Browser));
    assert_eq!(
        core.get_status(&added.id).unwrap().added_by,
        Some(AddedBy::Browser)
    );
    let plain = AddDownloadParams {
        url: server.url().join("other.bin").unwrap().to_string(),
        filename: Some("other.bin".to_string()),
        ..add_params(&server, folder.path(), None)
    };
    assert_eq!(core.add_download(plain).unwrap().added_by, None);
    stop(&core).await;
}

#[tokio::test]
async fn continuing_a_stopped_download_keeps_its_added_by() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let server = slow_server().await;
    let (core, paused) = paused_with_cookies(data.path(), folder.path(), &server).await;
    assert_eq!(paused.added_by, None);
    let from_browser = AddDownloadParams {
        added_by: Some(AddedBy::Browser),
        ..add_params(&server, folder.path(), Some(context("fresh")))
    };

    let again = core.add_download(from_browser).unwrap();

    assert_eq!(again.id, paused.id);
    assert_eq!(
        again.added_by, None,
        "a dashboard download stays out of the browser's reach"
    );
    core.cancel_download(CancelDownloadParams {
        id: again.id,
        delete_partial: true,
    })
    .unwrap();
}
