use rustdm::engine::{build_client, download, DownloadRequest, Outcome, Progress};
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn downloads_whole_file_when_range_ignored() {
    let size = 1024 * 1024 + 17;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::IgnoreRange).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("file.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 1,
        progress: Arc::clone(&progress),
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, steals } => {
            assert_eq!(steals, 0);
            assert_eq!(fs::read(path).unwrap(), body);
        }
    }
}

#[tokio::test]
async fn downloads_unknown_length_stream() {
    let body = pattern_bytes(5000);
    let srv = TestServer::start(body.clone(), Mode::Chunked).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("stream.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 1,
        progress: Arc::clone(&progress),
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(fs::read(path).unwrap(), body);
        }
    }
}

#[tokio::test]
async fn progress_reaches_total() {
    let body = pattern_bytes(10000);
    let srv = TestServer::start(body.clone(), Mode::IgnoreRange).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("test.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 1,
        progress: Arc::clone(&progress),
    };

    let _ = download(&client, req).await.unwrap();
    assert_eq!(progress.downloaded.load(Ordering::SeqCst), 10000);
}

#[tokio::test]
async fn never_overwrites_existing_file() {
    let body = pattern_bytes(100);
    let srv = TestServer::start(body.clone(), Mode::IgnoreRange).await;
    let dir = tempdir().unwrap();

    let existing_path = dir.path().join("x.bin");
    fs::write(&existing_path, b"original content").unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("x.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 1,
        progress: Arc::clone(&progress),
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(path.file_name().unwrap(), "x (1).bin");
            assert_eq!(fs::read(path).unwrap(), body);
            assert_eq!(fs::read(&existing_path).unwrap(), b"original content");
        }
    }
}
