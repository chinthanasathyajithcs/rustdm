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

#[tokio::test]
async fn multi_connection_download_matches_source() {
    let size = 8 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempdir().unwrap();

    for conns in [1, 2, 4, 8, 16] {
        let client = build_client();
        let progress = Arc::new(Progress::new());
        let req = DownloadRequest {
            url: srv.url().join(&format!("multi_{conns}.bin")).unwrap(),
            template: "{downloads}/{name}".to_string(),
            downloads_dir: dir.path().to_path_buf(),
            connections: conns,
            progress: Arc::clone(&progress),
        };

        let outcome = download(&client, req).await.unwrap();
        match outcome {
            Outcome::Completed { path, .. } => {
                assert_eq!(fs::read(path).unwrap(), body);
            }
        }
    }
}

#[tokio::test]
async fn small_file_uses_one_range_request() {
    let size = 100 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body, Mode::Normal).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("small.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 8,
        progress: Arc::clone(&progress),
    };

    let _ = download(&client, req).await.unwrap();
    let range_requests: Vec<_> = srv
        .requests()
        .into_iter()
        .filter(|r| r.range.is_some() && r.range.as_deref() != Some("bytes=0-0"))
        .collect();
    assert_eq!(range_requests.len(), 1);
}

#[tokio::test]
async fn every_range_request_sends_if_range_and_identity() {
    let size = 4 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body, Mode::Normal).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("range_headers.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 4,
        progress: Arc::clone(&progress),
    };

    let _ = download(&client, req).await.unwrap();
    let range_requests: Vec<_> = srv
        .requests()
        .into_iter()
        .filter(|r| r.range.is_some() && r.range.as_deref() != Some("bytes=0-0"))
        .collect();
    assert!(!range_requests.is_empty());
    for r in range_requests {
        assert_eq!(r.if_range.as_deref(), Some("\"v1\""));
        assert_eq!(r.accept_encoding.as_deref(), Some("identity"));
    }
}

#[tokio::test]
async fn fetch_range_reports_file_changed_when_validator_stale() {
    use rustdm::ranges::ByteRange;
    use rustdm::writer::PartFile;

    let size = 2 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body, Mode::Normal).await;
    srv.set_etag("\"v2\"");

    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("stale.bin.part");
    let part = Arc::new(PartFile::create(&part_path, Some(size)).unwrap());
    let progress = Arc::new(Progress::new());

    let validator = rustdm::probe::Validator {
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
    };

    let err = rustdm::worker::fetch_range(
        &client,
        &srv.url().join("stale.bin").unwrap(),
        &validator,
        ByteRange {
            start: 0,
            end: size,
        },
        part,
        progress,
    )
    .await
    .unwrap_err();

    match err {
        rustdm::error::Error::FileChanged => {}
        other => panic!("expected FileChanged, got {other:?}"),
    }
}
