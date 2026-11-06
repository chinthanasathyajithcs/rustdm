use rustdm::engine::{build_client, resume, Outcome};
use rustdm::error::Error;
use rustdm::probe::Validator;
use rustdm::ranges::ByteRange;
use rustdm::state::{part_path, save_atomic, state_path, State, STATE_VERSION};
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use rustdm::writer::PartFile;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn resume_completes_partial_download() {
    let size = 4 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempdir().unwrap();

    let output = dir.path().join("resume_test.bin");
    let part_p = part_path(&output);
    let state_p = state_path(&output);

    // Create part file and write first half
    let part = PartFile::create(&part_p, Some(size)).unwrap();
    let half = size / 2;
    part.write_at(0, &body[..half as usize]).unwrap();
    part.sync().unwrap();

    let state = State {
        version: STATE_VERSION,
        url: srv.url().join("resume_test.bin").unwrap().to_string(),
        final_url: srv.url().join("resume_test.bin").unwrap().to_string(),
        total: size,
        validator: Validator {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        },
        completed: vec![ByteRange {
            start: 0,
            end: half,
        }],
        output: output.clone(),
        connections: None,
    };
    save_atomic(&state_p, &state).unwrap();

    let client = build_client();
    let outcome = resume(&client, &state_p, None).await.unwrap();

    match outcome {
        Outcome::Completed { path, .. } => {
            assert_eq!(path, output);
            assert_eq!(fs::read(&output).unwrap(), body);
            assert!(!part_p.exists());
            assert!(!state_p.exists());
        }
        Outcome::Paused { .. } => panic!("expected Completed on resume"),
    }

    let range_requests: Vec<_> = srv
        .requests()
        .into_iter()
        .filter(|r| r.range.is_some() && r.range.as_deref() != Some("bytes=0-0"))
        .collect();
    assert!(!range_requests.is_empty());
    for req in range_requests {
        let range_str = req.range.unwrap();
        let start_str = range_str
            .strip_prefix("bytes=")
            .unwrap()
            .split('-')
            .next()
            .unwrap();
        let start: u64 = start_str.parse().unwrap();
        assert!(start >= half);
    }
}

#[tokio::test]
async fn resume_refuses_when_file_changed() {
    let size = 2 * 1024 * 1024;
    let body = pattern_bytes(size);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    srv.set_etag("\"v2\"");

    let dir = tempdir().unwrap();
    let output = dir.path().join("changed.bin");
    let part_p = part_path(&output);
    let state_p = state_path(&output);

    let part = PartFile::create(&part_p, Some(size)).unwrap();
    part.write_at(0, &body[..1000]).unwrap();
    part.sync().unwrap();

    let state = State {
        version: STATE_VERSION,
        url: srv.url().join("changed.bin").unwrap().to_string(),
        final_url: srv.url().join("changed.bin").unwrap().to_string(),
        total: size,
        validator: Validator {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        },
        completed: vec![ByteRange {
            start: 0,
            end: 1000,
        }],
        output: output.clone(),
        connections: None,
    };
    save_atomic(&state_p, &state).unwrap();

    let client = build_client();
    let err = resume(&client, &state_p, None).await.unwrap_err();
    match err {
        Error::FileChanged => {}
        other => panic!("expected FileChanged, got {other:?}"),
    }

    let read_back = fs::read(&part_p).unwrap();
    assert_eq!(&read_back[..1000], &body[..1000]);
    assert!(state_p.exists());
}

#[tokio::test]
async fn resume_errors_when_part_file_missing() {
    let size = 2 * 1024 * 1024;
    let dir = tempdir().unwrap();
    let output = dir.path().join("missing.bin");
    let state_p = state_path(&output);

    let state = State {
        version: STATE_VERSION,
        url: "http://127.0.0.1:12345/missing.bin".to_string(),
        final_url: "http://127.0.0.1:12345/missing.bin".to_string(),
        total: size,
        validator: Validator::default(),
        completed: vec![],
        output,
        connections: None,
    };
    save_atomic(&state_p, &state).unwrap();

    let client = build_client();
    let err = resume(&client, &state_p, None).await.unwrap_err();
    match err {
        Error::Io(_) => {}
        other => panic!("expected Io error, got {other:?}"),
    }
}

/// Write a partial download of `body` with `completed` already on disk.
fn partial_download(
    srv: &TestServer,
    dir: &std::path::Path,
    body: &[u8],
    completed: Vec<ByteRange>,
    connections: Option<u32>,
) -> std::path::PathBuf {
    let output = dir.join("lanes.bin");
    let part = PartFile::create(&part_path(&output), Some(body.len() as u64)).unwrap();
    for r in &completed {
        part.write_at(r.start, &body[r.start as usize..r.end as usize])
            .unwrap();
    }
    part.sync().unwrap();
    let url = srv.url().join("lanes.bin").unwrap().to_string();
    let state = State {
        version: STATE_VERSION,
        url: url.clone(),
        final_url: url,
        total: body.len() as u64,
        validator: Validator {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        },
        completed,
        output: output.clone(),
        connections,
    };
    let state_p = state_path(&output);
    save_atomic(&state_p, &state).unwrap();
    state_p
}

#[tokio::test]
async fn resume_spreads_one_gap_over_the_saved_connection_count() {
    const MIB: u64 = 1024 * 1024;
    let body = pattern_bytes(16 * MIB);
    let srv = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 16 * MIB,
        },
    )
    .await;
    let dir = tempdir().unwrap();
    let state_p = partial_download(
        &srv,
        dir.path(),
        &body,
        vec![ByteRange::new(0, 8 * MIB)],
        Some(4),
    );

    let outcome = resume(&build_client(), &state_p, None).await.unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    assert_eq!(fs::read(dir.path().join("lanes.bin")).unwrap(), body);
    assert_eq!(srv.peak_concurrent_requests(), 4);
}

#[tokio::test]
async fn resume_never_opens_more_connections_than_saved_even_with_many_gaps() {
    const MIB: u64 = 1024 * 1024;
    let body = pattern_bytes(12 * MIB);
    let srv = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 16 * MIB,
        },
    )
    .await;
    let dir = tempdir().unwrap();
    // Six separate gaps: every other MiB is already on disk.
    let completed = (0..6)
        .map(|i| ByteRange::new(2 * i * MIB, (2 * i + 1) * MIB))
        .collect();
    let state_p = partial_download(&srv, dir.path(), &body, completed, Some(2));

    let outcome = resume(&build_client(), &state_p, None).await.unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    assert_eq!(fs::read(dir.path().join("lanes.bin")).unwrap(), body);
    assert_eq!(srv.peak_concurrent_requests(), 2);
}
