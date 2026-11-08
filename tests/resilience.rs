//! How the engine copes with bad networks, unusual servers, and missing folders.

use rustdm::engine::{
    build_client, build_client_with, download, resume, DownloadRequest, Outcome, Progress, Timeouts,
};
use rustdm::probe::Validator;
use rustdm::ranges::ByteRange;
use rustdm::state::{part_path, save_atomic, state_path, State, STATE_VERSION};
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use rustdm::writer::PartFile;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const KIB: u64 = 1024;

/// A client that gives up on a silent connection after 300 ms.
fn impatient_client() -> reqwest::Client {
    build_client_with(Timeouts {
        connect: Duration::from_secs(5),
        read: Duration::from_millis(300),
    })
}

fn request(srv: &TestServer, name: &str, template: &str, dir: &Path) -> DownloadRequest {
    DownloadRequest {
        url: srv.url().join(name).unwrap(),
        template: template.to_string(),
        file_name: None,
        downloads_dir: dir.to_path_buf(),
        connections: 1,
        progress: Arc::new(Progress::new()),
        cancel: None,
    }
}

fn completed_path(outcome: Outcome) -> std::path::PathBuf {
    match outcome {
        Outcome::Completed { path, .. } => path,
        other => panic!("expected completion, got {other:?}"),
    }
}

#[tokio::test]
async fn a_stalled_connection_is_dropped_and_reopened_until_the_file_is_done() {
    let body = pattern_bytes(256 * KIB);
    let srv = TestServer::start(body.clone(), Mode::Stall { after: 96 * KIB }).await;
    let dir = tempfile::tempdir().unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        download(
            &impatient_client(),
            request(&srv, "stall.bin", "{downloads}/{name}", dir.path()),
        ),
    )
    .await
    .expect("the download hung on a silent connection")
    .unwrap();
    assert_eq!(std::fs::read(completed_path(outcome)).unwrap(), body);
}

#[tokio::test]
async fn pause_takes_effect_while_a_connection_is_stalled() {
    let srv = TestServer::start(pattern_bytes(256 * KIB), Mode::Stall { after: 16 * KIB }).await;
    let dir = tempfile::tempdir().unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        progress: Arc::clone(&progress),
        cancel: Some(cancel_rx),
        ..request(&srv, "stall.bin", "{downloads}/{name}", dir.path())
    };
    // The default client waits 30 s before calling a connection stalled.
    let handle = tokio::spawn(async move { download(&build_client(), req).await });
    while progress.downloaded.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let paused_at = Instant::now();
    cancel_tx.send(true).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("pause was ignored while the connection was silent")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
    assert!(paused_at.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn a_connection_that_keeps_dropping_still_finishes_while_it_makes_progress() {
    let body = pattern_bytes(1024 * KIB);
    // Seven drops: more than the five attempts a connection gets without progress.
    let srv = TestServer::start(body.clone(), Mode::DropEvery { bytes: 128 * KIB }).await;
    let dir = tempfile::tempdir().unwrap();

    let outcome = download(
        &build_client(),
        request(&srv, "flaky.bin", "{downloads}/{name}", dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(completed_path(outcome)).unwrap(), body);
}

#[tokio::test]
async fn missing_folders_in_the_output_path_are_created() {
    let body = pattern_bytes(64 * KIB);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let root = tempfile::tempdir().unwrap();
    let downloads = root.path().join("not-yet").join("Downloads");

    let outcome = download(
        &build_client(),
        request(&srv, "nested.bin", "{downloads}/{host}/{name}", &downloads),
    )
    .await
    .unwrap();
    let path = completed_path(outcome);
    assert_eq!(path.parent().unwrap().parent().unwrap(), downloads);
    assert_eq!(std::fs::read(path).unwrap(), body);
}

#[tokio::test]
async fn a_folder_that_cannot_be_created_is_named_in_the_error() {
    let srv = TestServer::start(pattern_bytes(64 * KIB), Mode::Normal).await;
    let root = tempfile::tempdir().unwrap();
    let blocker = root.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a folder").unwrap();

    let err = download(
        &build_client(),
        request(&srv, "x.bin", "{downloads}/{name}", &blocker.join("sub")),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains(&*blocker.to_string_lossy()), "{err}");
}

/// Leave `body[..have]` on disk as a paused download of `body`.
fn paused_download(
    srv: &TestServer,
    dir: &Path,
    body: &[u8],
    have: u64,
    total: u64,
) -> std::path::PathBuf {
    let output = dir.join("restart.bin");
    let part = PartFile::create(&part_path(&output), Some(body.len() as u64)).unwrap();
    part.write_at(0, &body[..have as usize]).unwrap();
    part.sync().unwrap();
    let url = srv.url().join("restart.bin").unwrap().to_string();
    let state = State {
        version: STATE_VERSION,
        url: url.clone(),
        final_url: url,
        total,
        validator: Validator {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        },
        completed: vec![ByteRange::new(0, have)],
        output,
        connections: Some(2),
    };
    let state_p = state_path(&state.output);
    save_atomic(&state_p, &state).unwrap();
    state_p
}

#[tokio::test]
async fn resume_starts_over_when_the_server_cannot_send_part_of_the_file() {
    let body = pattern_bytes(512 * KIB);
    let srv = TestServer::start(body.clone(), Mode::IgnoreRange).await;
    let dir = tempfile::tempdir().unwrap();
    let state_p = paused_download(&srv, dir.path(), &body, 128 * KIB, body.len() as u64);

    let outcome = resume(&build_client(), &state_p, None).await.unwrap();
    assert_eq!(std::fs::read(completed_path(outcome)).unwrap(), body);
    assert!(!state_p.exists());
}

#[tokio::test]
async fn resume_starts_over_when_the_size_was_never_known() {
    let body = pattern_bytes(512 * KIB);
    let srv = TestServer::start(body.clone(), Mode::Chunked).await;
    let dir = tempfile::tempdir().unwrap();
    let state_p = paused_download(&srv, dir.path(), &body, 128 * KIB, 0);

    let outcome = resume(&build_client(), &state_p, None).await.unwrap();
    assert_eq!(std::fs::read(completed_path(outcome)).unwrap(), body);
}

#[tokio::test]
async fn resume_still_refuses_a_file_that_really_changed() {
    let body = pattern_bytes(512 * KIB);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let state_p = paused_download(&srv, dir.path(), &body, 128 * KIB, body.len() as u64 + 1);

    let err = resume(&build_client(), &state_p, None).await.unwrap_err();
    assert!(matches!(err, rustdm::error::Error::FileChanged), "{err:?}");
}
