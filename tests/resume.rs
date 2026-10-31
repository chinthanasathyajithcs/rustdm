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
    };
    save_atomic(&state_p, &state).unwrap();

    let client = build_client();
    let err = resume(&client, &state_p, None).await.unwrap_err();
    match err {
        Error::Io(_) => {}
        other => panic!("expected Io error, got {other:?}"),
    }
}
