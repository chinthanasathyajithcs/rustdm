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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, steals } => {
            assert_eq!(steals, 0);
            assert_eq!(fs::read(path).unwrap(), body);
        }
        Outcome::Paused { .. } => panic!("expected completed outcome"),
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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(fs::read(path).unwrap(), body);
        }
        Outcome::Paused { .. } => panic!("expected completed outcome"),
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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(path.file_name().unwrap(), "x (1).bin");
            assert_eq!(fs::read(path).unwrap(), body);
            assert_eq!(fs::read(&existing_path).unwrap(), b"original content");
        }
        Outcome::Paused { .. } => panic!("expected completed outcome"),
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
            cancel: None,
            file_name: None,
            max_request_bytes: None,
        };

        let outcome = download(&client, req).await.unwrap();
        match outcome {
            Outcome::Completed { path, .. } => {
                assert_eq!(fs::read(path).unwrap(), body);
            }
            Outcome::Paused { .. } => panic!("expected completed outcome"),
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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
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
        cancel: None,
        file_name: None,
        max_request_bytes: None,
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

#[tokio::test]
async fn honors_retry_after_on_503() {
    let size = 1000;
    let body = pattern_bytes(size);
    let srv = TestServer::start(
        body.clone(),
        Mode::FailThenOk {
            status: 503,
            times: 2,
            retry_after: Some(0),
        },
    )
    .await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("retry503.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 1,
        progress: Arc::clone(&progress),
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(fs::read(path).unwrap(), body);
        }
        Outcome::Paused { .. } => panic!("expected completed outcome"),
    }
}

#[tokio::test]
async fn link_expired_fails_without_retry() {
    let srv = TestServer::start(vec![], Mode::Status(403)).await;
    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("link_exp.bin.part");
    let part = Arc::new(rustdm::writer::PartFile::create(&part_path, Some(1000)).unwrap());
    let progress = Arc::new(Progress::new());
    let validator = rustdm::probe::Validator::default();

    let err = rustdm::worker::fetch_range(
        &client,
        &srv.url().join("test.bin").unwrap(),
        &validator,
        rustdm::ranges::ByteRange {
            start: 0,
            end: 1000,
        },
        part,
        progress,
    )
    .await
    .unwrap_err();

    match err {
        rustdm::error::Error::LinkExpired { status } => assert_eq!(status, 403),
        other => panic!("expected LinkExpired, got {other:?}"),
    }
    assert_eq!(srv.requests().len(), 1);
}

#[tokio::test]
async fn sign_in_required_mid_download_is_link_expired_without_retry() {
    let srv = TestServer::start(vec![], Mode::Status(401)).await;
    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("signin.bin.part");
    let part = Arc::new(rustdm::writer::PartFile::create(&part_path, Some(1000)).unwrap());
    let progress = Arc::new(Progress::new());

    let err = rustdm::worker::fetch_range(
        &client,
        &srv.url().join("test.bin").unwrap(),
        &rustdm::probe::Validator::default(),
        rustdm::ranges::ByteRange {
            start: 0,
            end: 1000,
        },
        part,
        progress,
    )
    .await
    .unwrap_err();

    match err {
        rustdm::error::Error::LinkExpired { status } => assert_eq!(status, 401),
        other => panic!("expected LinkExpired, got {other:?}"),
    }
    assert_eq!(srv.requests().len(), 1);
}

fn request_for(url: url::Url, dir: &std::path::Path) -> DownloadRequest {
    DownloadRequest {
        url,
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.to_path_buf(),
        connections: 2,
        progress: Arc::new(Progress::new()),
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    }
}

async fn serve_with_type(content_type: Option<&str>) -> TestServer {
    let mode = match content_type {
        Some(value) => Mode::ContentType(value.to_string()),
        None => Mode::Normal,
    };
    TestServer::start(pattern_bytes(4096), mode).await
}

#[tokio::test]
async fn a_web_page_is_refused_and_leaves_no_file() {
    for content_type in [
        "text/html",
        "text/html; charset=UTF-8",
        "Application/XHTML+XML",
    ] {
        let srv = serve_with_type(Some(content_type)).await;
        let dir = tempdir().unwrap();

        let err = download(
            &build_client(),
            request_for(srv.url().join("watch").unwrap(), dir.path()),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, rustdm::error::Error::WebPage),
            "{content_type}: got {err:?}"
        );
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "{content_type}"
        );
    }
}

#[tokio::test]
async fn a_link_that_names_a_page_file_still_downloads() {
    for name in ["index.html", "INDEX.HTM", "a.xhtml", "saved.mhtml"] {
        let srv = serve_with_type(Some("text/html; charset=utf-8")).await;
        let dir = tempdir().unwrap();

        let outcome = download(
            &build_client(),
            request_for(srv.url().join(name).unwrap(), dir.path()),
        )
        .await
        .unwrap();

        match outcome {
            Outcome::Completed { path, .. } => assert_eq!(fs::read(path).unwrap().len(), 4096),
            Outcome::Paused { .. } => panic!("{name}: expected completed outcome"),
        }
    }
}

#[tokio::test]
async fn binary_and_untyped_responses_download_as_before() {
    for content_type in [Some("application/octet-stream"), None] {
        let srv = serve_with_type(content_type).await;
        let dir = tempdir().unwrap();

        let outcome = download(
            &build_client(),
            request_for(srv.url().join("file.bin").unwrap(), dir.path()),
        )
        .await
        .unwrap();

        match outcome {
            Outcome::Completed { path, .. } => {
                assert_eq!(fs::read(path).unwrap(), pattern_bytes(4096))
            }
            Outcome::Paused { .. } => panic!("{content_type:?}: expected completed outcome"),
        }
    }
}

#[tokio::test]
async fn short_bodies_are_retried_while_each_one_adds_bytes() {
    // The server sends half of what is left each time: 8, 4, 2, 1, then 1 byte.
    // Every attempt makes progress, so the range finishes instead of giving up.
    let body = pattern_bytes(16);
    let srv = TestServer::start(body.clone(), Mode::ShortBody).await;
    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("short.bin.part");
    let part = Arc::new(rustdm::writer::PartFile::create(&part_path, Some(16)).unwrap());
    let progress = Arc::new(Progress::new());
    let validator = rustdm::probe::Validator::default();

    rustdm::worker::fetch_range(
        &client,
        &srv.url().join("short.bin").unwrap(),
        &validator,
        rustdm::ranges::ByteRange { start: 0, end: 16 },
        part,
        Arc::clone(&progress),
    )
    .await
    .unwrap();

    assert_eq!(fs::read(&part_path).unwrap(), body);
    let range_reqs = srv
        .requests()
        .into_iter()
        .filter(|r| r.range.is_some())
        .count();
    assert_eq!(range_reqs, 5);
}

#[tokio::test]
async fn wrong_content_range_is_protocol_error() {
    let body = pattern_bytes(2000);
    let srv = TestServer::start(body, Mode::WrongContentRange).await;
    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("wrong_cr.bin.part");
    let part = Arc::new(rustdm::writer::PartFile::create(&part_path, Some(2000)).unwrap());
    let progress = Arc::new(Progress::new());
    let validator = rustdm::probe::Validator::default();

    let err = rustdm::worker::fetch_range(
        &client,
        &srv.url().join("wrong_cr.bin").unwrap(),
        &validator,
        rustdm::ranges::ByteRange {
            start: 0,
            end: 2000,
        },
        part,
        progress,
    )
    .await
    .unwrap_err();

    match err {
        rustdm::error::Error::Protocol(_) => {}
        other => panic!("expected Protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn resumes_from_last_written_byte_after_drop() {
    let size = 2000;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::DropMidBodyOnce).await;
    let client = build_client();
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("drop.bin.part");
    let part = Arc::new(rustdm::writer::PartFile::create(&part_path, Some(size)).unwrap());
    let progress = Arc::new(Progress::new());
    let validator = rustdm::probe::Validator::default();

    rustdm::worker::fetch_range(
        &client,
        &srv.url().join("drop.bin").unwrap(),
        &validator,
        rustdm::ranges::ByteRange {
            start: 0,
            end: size,
        },
        Arc::clone(&part),
        progress,
    )
    .await
    .unwrap();

    let logs: Vec<_> = srv
        .requests()
        .into_iter()
        .filter(|r| r.range.is_some() && r.range.as_deref() != Some("bytes=0-0"))
        .collect();
    assert_eq!(logs.len(), 2);
    let second_range = logs[1].range.as_deref().unwrap();
    assert!(second_range.starts_with("bytes="));
    let start_byte: u64 = second_range
        .strip_prefix("bytes=")
        .unwrap()
        .split('-')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(start_byte > 0);
    assert_eq!(fs::read(&part_path).unwrap(), body);
}

#[tokio::test]
async fn stealing_with_slow_low_half_is_byte_identical() {
    let size = 8 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(
        body.clone(),
        Mode::SlowLowHalf {
            bytes_per_sec: 1024 * 1024,
        },
    )
    .await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("steal.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 2,
        progress: Arc::clone(&progress),
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Completed { path, steals } => {
            assert!(steals >= 1, "expected at least 1 steal, got {steals}");
            assert_eq!(fs::read(path).unwrap(), body);
        }
        Outcome::Paused { .. } => panic!("expected completed outcome"),
    }
}

#[tokio::test]
async fn cancel_pauses_and_state_resumes() {
    let size = 8 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 2 * 1024 * 1024,
        },
    )
    .await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let req = DownloadRequest {
        url: srv.url().join("cancel.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 4,
        progress: Arc::clone(&progress),
        cancel: Some(cancel_rx),
        file_name: None,
        max_request_bytes: None,
    };

    let download_handle = tokio::spawn(async move { download(&client, req).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel_tx.send(true).unwrap();

    let outcome = download_handle.await.unwrap().unwrap();
    let state_path = match outcome {
        Outcome::Paused { reason, state } => {
            assert_eq!(reason, rustdm::engine::PauseReason::Cancelled);
            assert!(state.exists());
            state
        }
        Outcome::Completed { .. } => panic!("expected Paused, got Completed"),
    };

    let resume_client = build_client();
    let resume_outcome = rustdm::engine::resume(&resume_client, &state_path, None)
        .await
        .unwrap();
    match resume_outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(std::fs::read(path).unwrap(), body);
        }
        Outcome::Paused { .. } => panic!("expected Completed on resume"),
    }
}

#[tokio::test]
async fn link_expired_pauses_with_state_saved() {
    let size = 8 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::ExpireAfter(3)).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());

    let req = DownloadRequest {
        url: srv.url().join("expire.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 4,
        progress: Arc::clone(&progress),
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let outcome = download(&client, req).await.unwrap();
    match outcome {
        Outcome::Paused { reason, state } => {
            assert_eq!(
                reason,
                rustdm::engine::PauseReason::LinkExpired { status: 403 }
            );
            assert!(state.exists());
        }
        Outcome::Completed { .. } => panic!("expected Paused, got Completed"),
    }
}

#[tokio::test]
async fn pre_flight_disk_space_check_aborts_before_creating_part_file() {
    let fake_huge_size = u64::MAX / 2;
    let srv = TestServer::start(vec![0u8; 100], Mode::SimulateSize(fake_huge_size)).await;
    let dir = tempdir().unwrap();

    let client = build_client();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("huge_file.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 4,
        progress: Arc::clone(&progress),
        cancel: None,
        file_name: None,
        max_request_bytes: None,
    };

    let err = download(&client, req).await.unwrap_err();
    match err {
        rustdm::error::Error::InsufficientDiskSpace {
            target,
            required,
            available,
        } => {
            assert!(target.contains("huge_file.bin.part"));
            assert!(required >= fake_huge_size);
            assert!(available < required);
        }
        other => panic!("expected InsufficientDiskSpace, got {other:?}"),
    }

    // Crucial guarantee: verify NO part file or state file was created on disk!
    let part_path = dir.path().join("huge_file.bin.part");
    assert!(
        !part_path.exists(),
        "part file must NOT be created when disk space is insufficient"
    );
    let state_path = dir.path().join("huge_file.bin.part.rdm");
    assert!(
        !state_path.exists(),
        "state file must NOT be created when disk space is insufficient"
    );
}

#[tokio::test]
async fn paused_download_remembers_its_connection_count() {
    let body = pattern_bytes(8 * 1024 * 1024);
    let srv = TestServer::start(
        body,
        Mode::Throttle {
            bytes_per_sec: 1024 * 1024,
        },
    )
    .await;
    let dir = tempdir().unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest {
        url: srv.url().join("three.bin").unwrap(),
        template: "{downloads}/{name}".to_string(),
        downloads_dir: dir.path().to_path_buf(),
        connections: 3,
        progress: Arc::clone(&progress),
        cancel: Some(cancel_rx),
        file_name: None,
        max_request_bytes: None,
    };

    let handle = tokio::spawn(async move { download(&build_client(), req).await });
    while progress.downloaded.load(Ordering::Relaxed) == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    cancel_tx.send(true).unwrap();

    let state_file = match handle.await.unwrap().unwrap() {
        Outcome::Paused { state, .. } => state,
        other => panic!("expected a pause, got {other:?}"),
    };
    let saved = rustdm::state::load(&state_file).unwrap();
    assert_eq!(saved.connections, Some(3));
}
