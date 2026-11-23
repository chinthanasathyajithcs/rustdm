//! A download can ask the server for at most N bytes per request (YouTube refuses more).

use rustdm::engine::{
    build_client, download, resume_with, DownloadRequest, Outcome, Progress, ResumeOptions,
};
use rustdm::testing::{pattern_bytes, Mode, RequestLog, TestServer};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const FILE_SIZE: u64 = 1_000_000;
const CAP: u64 = 100_000;
/// Two lanes at 2 connections: the engine splits a file only in whole MiB.
const TWO_LANE_FILE_SIZE: u64 = 2 * 1024 * 1024;
/// The engine's size probe, which is not part of the file.
const PROBE_RANGE: &str = "bytes=0-0";

fn request(srv: &TestServer, dir: &std::path::Path, cap: Option<u64>) -> DownloadRequest {
    DownloadRequest {
        url: srv.url().join("capped.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        file_name: Some("capped.bin".to_string()),
        downloads_dir: dir.to_path_buf(),
        connections: 2,
        progress: Arc::new(Progress::new()),
        cancel: None,
        max_request_bytes: cap,
    }
}

/// Byte counts of the ranges asked for, in the order they arrived.
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

fn completed(outcome: Outcome) -> std::path::PathBuf {
    match outcome {
        Outcome::Completed { path, .. } => path,
        Outcome::Paused { reason, .. } => panic!("download paused: {reason:?}"),
    }
}

#[tokio::test]
async fn a_capped_download_never_asks_for_more_than_the_cap() {
    let body = pattern_bytes(FILE_SIZE);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();

    let outcome = download(&build_client(), request(&srv, dir.path(), Some(CAP)))
        .await
        .unwrap();

    let sizes = range_sizes(&srv.requests());
    assert!(sizes.len() >= 10, "{sizes:?}");
    assert!(sizes.iter().all(|&size| size <= CAP), "{sizes:?}");
    assert_eq!(std::fs::read(completed(outcome)).unwrap(), body);
}

#[tokio::test]
async fn an_uncapped_download_asks_once_per_lane() {
    let body = pattern_bytes(TWO_LANE_FILE_SIZE);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();

    let outcome = download(&build_client(), request(&srv, dir.path(), None))
        .await
        .unwrap();

    assert_eq!(range_sizes(&srv.requests()).len(), 2);
    assert_eq!(std::fs::read(completed(outcome)).unwrap(), body);
}

#[tokio::test]
async fn a_capped_resume_keeps_the_cap() {
    let body = pattern_bytes(FILE_SIZE);
    // Slow enough to cancel part-way: 2 connections at 400 kB/s each.
    let srv = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 400_000,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let first = DownloadRequest {
        cancel: Some(cancel_rx),
        ..request(&srv, dir.path(), Some(CAP))
    };
    let progress = Arc::clone(&first.progress);
    let running = tokio::spawn(async move { download(&build_client(), first).await });
    while progress.downloaded.load(Ordering::SeqCst) < 300_000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel_tx.send(true).unwrap();
    let paused = running.await.unwrap().unwrap();
    let Outcome::Paused { state, .. } = paused else {
        panic!("the download finished before it was cancelled: {paused:?}");
    };
    let requests_before_resume = srv.requests().len();

    let opts = ResumeOptions {
        max_request_bytes: Some(CAP),
        ..ResumeOptions::default()
    };
    let outcome = resume_with(&build_client(), &state, opts).await.unwrap();

    let sizes = range_sizes(&srv.requests()[requests_before_resume..]);
    assert!(!sizes.is_empty());
    assert!(sizes.iter().all(|&size| size <= CAP), "{sizes:?}");
    assert_eq!(std::fs::read(completed(outcome)).unwrap(), body);
}
