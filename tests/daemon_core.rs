use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::{DaemonCore, DaemonServer};
use rustdm::dirs::default_downloads_dir;
use rustdm::ipc::client::DaemonClient;
use rustdm::ipc::protocol::{
    AddDownloadParams, CancelDownloadParams, DownloadIdParam, DownloadStatus, DownloadSummary,
    PingResult, RpcId, RpcMessage, RpcRequest, SetDefaultsParams, CODE_DOWNLOAD_NOT_FOUND,
    CODE_INVALID_PARAMS, CODE_INVALID_PATH, CODE_PERMISSION_DENIED,
};
use rustdm::ipc::transport::{IpcConnection, IpcEndpoint};
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(100);

fn next_test_endpoint() -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::test_endpoint(&format!("daemon_{}", id))
}

#[tokio::test]
async fn test_daemon_ping_and_list_empty() {
    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind(&endpoint).await.expect("bind daemon");
    let server_task = tokio::spawn(async move {
        server.run().await.unwrap();
    });

    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");

    // 1. Ping
    let req = RpcRequest::new(RpcId::Number(1), "daemon.ping", None);
    client.send(&req).await.expect("send ping");
    let resp = client.recv().await.unwrap().unwrap();
    match resp {
        RpcMessage::Response(r) => {
            assert_eq!(r.id, RpcId::Number(1));
            let ping: PingResult = serde_json::from_value(r.result.unwrap()).unwrap();
            assert_eq!(ping.version, rustdm::VERSION);
        }
        _ => panic!("expected response"),
    }

    // 2. List (should be empty initially)
    let list_req = RpcRequest::new(RpcId::Number(2), "download.list", None);
    client.send(&list_req).await.expect("send list");
    let resp = client.recv().await.unwrap().unwrap();
    match resp {
        RpcMessage::Response(r) => {
            let list: Vec<DownloadSummary> = serde_json::from_value(r.result.unwrap()).unwrap();
            assert!(list.is_empty());
        }
        _ => panic!("expected response"),
    }

    // 3. Shutdown
    let shutdown_req = RpcRequest::new(RpcId::Number(3), "daemon.shutdown", None);
    client.send(&shutdown_req).await.expect("send shutdown");
    let _ = client.recv().await.unwrap();

    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn test_daemon_add_download_and_complete() {
    let mock_bytes = pattern_bytes(512 * 1024);
    let mock = TestServer::start(mock_bytes.clone(), Mode::Normal).await;

    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind(&endpoint).await.expect("bind daemon");
    let server_task = tokio::spawn(async move {
        server.run().await.unwrap();
    });

    let tmp_dir = tempfile::tempdir().unwrap();
    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");

    let add_params = AddDownloadParams {
        url: mock.url().to_string(),
        destination: Some(tmp_dir.path().to_path_buf()),
        filename: Some("downloaded.bin".to_string()),
        connections: Some(4),
        max_bytes_per_sec: None,
    };
    let add_req = RpcRequest::new(
        RpcId::Number(10),
        "download.add",
        Some(serde_json::to_value(&add_params).unwrap()),
    );
    client.send(&add_req).await.unwrap();

    let resp = client.recv().await.unwrap().unwrap();
    let summary: DownloadSummary = match resp {
        RpcMessage::Response(r) => serde_json::from_value(r.result.unwrap()).unwrap(),
        _ => panic!("expected response"),
    };
    let dl_id = summary.id;

    // Poll status until completed (timeout after 10s)
    let final_path = tmp_dir.path().join("downloaded.bin");
    let start = std::time::Instant::now();
    let mut completed = false;
    while start.elapsed() < Duration::from_secs(10) {
        let status_req = RpcRequest::new(
            RpcId::Number(11),
            "download.status",
            Some(serde_json::to_value(DownloadIdParam { id: dl_id.clone() }).unwrap()),
        );
        client.send(&status_req).await.unwrap();
        let resp = client.recv().await.unwrap().unwrap();
        if let RpcMessage::Response(r) = resp {
            let s: DownloadSummary = serde_json::from_value(r.result.unwrap()).unwrap();
            if s.status == DownloadStatus::Completed {
                completed = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(completed, "download did not complete in time");
    assert!(final_path.exists());
    let read_back = std::fs::read(&final_path).unwrap();
    assert_eq!(read_back, mock_bytes);

    // Shutdown daemon
    let shutdown_req = RpcRequest::new(RpcId::Number(99), "daemon.shutdown", None);
    client.send(&shutdown_req).await.unwrap();
    let _ = server_task.await;
}

#[tokio::test]
async fn test_daemon_pause_and_resume() {
    let mock_bytes = pattern_bytes(2 * 1024 * 1024); // 2 MB
    let mock = TestServer::start(mock_bytes.clone(), Mode::Normal).await;

    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind(&endpoint).await.expect("bind daemon");
    let server_task = tokio::spawn(async move {
        server.run().await.unwrap();
    });

    let tmp_dir = tempfile::tempdir().unwrap();
    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");

    let add_params = AddDownloadParams {
        url: mock.url().to_string(),
        destination: Some(tmp_dir.path().to_path_buf()),
        filename: Some("pause_resume.bin".to_string()),
        connections: Some(2),
        max_bytes_per_sec: None,
    };
    let add_req = RpcRequest::new(
        RpcId::Number(20),
        "download.add",
        Some(serde_json::to_value(&add_params).unwrap()),
    );
    client.send(&add_req).await.unwrap();
    let summary: DownloadSummary = match client.recv().await.unwrap().unwrap() {
        RpcMessage::Response(r) => serde_json::from_value(r.result.unwrap()).unwrap(),
        _ => panic!("expected response"),
    };
    let dl_id = summary.id;

    // Immediately pause
    let pause_req = RpcRequest::new(
        RpcId::Number(21),
        "download.pause",
        Some(serde_json::to_value(DownloadIdParam { id: dl_id.clone() }).unwrap()),
    );
    client.send(&pause_req).await.unwrap();
    let pause_resp = client.recv().await.unwrap().unwrap();
    if let RpcMessage::Response(r) = pause_resp {
        assert!(r.error.is_none());
    }

    // Wait for paused status
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let st_req = RpcRequest::new(
            RpcId::Number(22),
            "download.status",
            Some(serde_json::to_value(DownloadIdParam { id: dl_id.clone() }).unwrap()),
        );
        client.send(&st_req).await.unwrap();
        if let RpcMessage::Response(r) = client.recv().await.unwrap().unwrap() {
            let s: DownloadSummary = serde_json::from_value(r.result.unwrap()).unwrap();
            if s.status == DownloadStatus::Paused {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Resume
    let resume_req = RpcRequest::new(
        RpcId::Number(23),
        "download.resume",
        Some(serde_json::to_value(DownloadIdParam { id: dl_id.clone() }).unwrap()),
    );
    client.send(&resume_req).await.unwrap();
    let resume_resp = client.recv().await.unwrap().unwrap();
    if let RpcMessage::Response(r) = resume_resp {
        assert!(r.error.is_none());
    }

    // Wait for completed
    let start = std::time::Instant::now();
    let mut finished = false;
    while start.elapsed() < Duration::from_secs(10) {
        let st_req = RpcRequest::new(
            RpcId::Number(24),
            "download.status",
            Some(serde_json::to_value(DownloadIdParam { id: dl_id.clone() }).unwrap()),
        );
        client.send(&st_req).await.unwrap();
        if let RpcMessage::Response(r) = client.recv().await.unwrap().unwrap() {
            let s: DownloadSummary = serde_json::from_value(r.result.unwrap()).unwrap();
            if s.status == DownloadStatus::Completed {
                finished = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(finished, "download did not complete after resume");
    let final_path = tmp_dir.path().join("pause_resume.bin");
    assert_eq!(std::fs::read(&final_path).unwrap(), mock_bytes);

    let shutdown_req = RpcRequest::new(RpcId::Number(99), "daemon.shutdown", None);
    client.send(&shutdown_req).await.unwrap();
    let _ = server_task.await;
}

#[tokio::test]
async fn test_daemon_error_scenarios() {
    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind(&endpoint).await.expect("bind daemon");
    let server_task = tokio::spawn(async move {
        server.run().await.unwrap();
    });

    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");

    // 1. Pause non-existent download
    let pause_req = RpcRequest::new(
        RpcId::Number(30),
        "download.pause",
        Some(
            serde_json::to_value(DownloadIdParam {
                id: "dl_missing".to_string(),
            })
            .unwrap(),
        ),
    );
    client.send(&pause_req).await.unwrap();
    let resp = client.recv().await.unwrap().unwrap();
    match resp {
        RpcMessage::Response(r) => {
            let err = r.error.expect("expected error");
            assert_eq!(err.code, CODE_DOWNLOAD_NOT_FOUND);
        }
        _ => panic!("expected response"),
    }

    // Shutdown
    let shutdown_req = RpcRequest::new(RpcId::Number(99), "daemon.shutdown", None);
    client.send(&shutdown_req).await.unwrap();
    let _ = server_task.await;
}

#[tokio::test]
async fn test_daemon_telemetry_streaming() {
    let mock_bytes = pattern_bytes(2 * 1024 * 1024);
    let mock = TestServer::start(
        mock_bytes.clone(),
        Mode::Throttle {
            bytes_per_sec: 500 * 1024,
        },
    )
    .await;

    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind(&endpoint).await.expect("bind daemon");
    let server_task = tokio::spawn(async move {
        server.run().await.unwrap();
    });

    let tmp_dir = tempfile::tempdir().unwrap();
    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");

    // 1. Subscribe to telemetry
    let sub_req = RpcRequest::new(RpcId::Number(40), "subscribe.telemetry", None);
    client.send(&sub_req).await.unwrap();
    let sub_resp = client.recv().await.unwrap().unwrap();
    if let RpcMessage::Response(r) = sub_resp {
        assert!(r.error.is_none());
    }

    // 2. Add download
    let add_params = AddDownloadParams {
        url: mock.url().to_string(),
        destination: Some(tmp_dir.path().to_path_buf()),
        filename: Some("telemetry_file.bin".to_string()),
        connections: Some(2),
        max_bytes_per_sec: None,
    };
    let add_req = RpcRequest::new(
        RpcId::Number(41),
        "download.add",
        Some(serde_json::to_value(&add_params).unwrap()),
    );
    client.send(&add_req).await.unwrap();

    // 3. Receive add response
    let add_resp = client.recv().await.unwrap().unwrap();
    match add_resp {
        RpcMessage::Response(r) => assert!(r.error.is_none()),
        _ => panic!("expected response"),
    }

    // 4. Client should receive telemetry notifications
    let mut got_tick = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Ok(Some(RpcMessage::Notification(n)))) =
            tokio::time::timeout(Duration::from_millis(500), client.recv()).await
        {
            if n.method == "telemetry.tick" {
                got_tick = true;
                break;
            }
        }
    }

    // Shutdown
    let shutdown_req = RpcRequest::new(RpcId::Number(99), "daemon.shutdown", None);
    client.send(&shutdown_req).await.unwrap();
    let _ = server_task.await;

    assert!(
        got_tick,
        "expected at least one telemetry.tick notification"
    );
}

#[test]
fn defaults_start_as_the_downloads_folder_and_eight_connections() {
    let defaults = DaemonCore::new().get_defaults();
    assert_eq!(defaults.folder, default_downloads_dir());
    assert_eq!(defaults.connections, 8);
    assert_eq!(defaults.max_bytes_per_sec, 0);
}

#[test]
fn set_defaults_changes_only_the_fields_given() {
    let core = DaemonCore::new();
    let dir = tempfile::tempdir().unwrap();
    let after = core
        .set_defaults(SetDefaultsParams {
            folder: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(after.folder, dir.path());
    assert_eq!(after.connections, 8);

    let after = core
        .set_defaults(SetDefaultsParams {
            connections: Some(4),
            max_bytes_per_sec: Some(2 * 1024 * 1024),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(after.folder, dir.path(), "folder kept");
    assert_eq!(after.connections, 4);
    assert_eq!(after.max_bytes_per_sec, 2 * 1024 * 1024);
    assert_eq!(core.get_defaults(), after);
}

fn rejection(core: &DaemonCore, params: SetDefaultsParams) -> String {
    let err = core.set_defaults(params).expect_err("must be rejected");
    assert_eq!(err.code, CODE_INVALID_PARAMS);
    err.message
}

#[test]
fn bad_defaults_are_rejected_with_the_field_named() {
    let core = DaemonCore::new();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let set = |folder: Option<std::path::PathBuf>, connections: Option<u32>, speed: Option<u64>| {
        SetDefaultsParams {
            folder,
            connections,
            max_bytes_per_sec: speed,
        }
    };

    for n in [0, 17] {
        assert_eq!(
            rejection(&core, set(None, Some(n), None)),
            "connections: must be between 1 and 16"
        );
    }
    assert_eq!(
        rejection(&core, set(Some("relative/dir".into()), None, None)),
        "folder: must be a full path"
    );
    assert_eq!(
        rejection(&core, set(Some(file), None, None)),
        "folder: is a file, not a folder"
    );
    assert!(rejection(&core, set(None, None, Some(100))).starts_with("speed: "));
    assert_eq!(core.get_defaults().connections, 8, "nothing was changed");
}

#[cfg(unix)]
#[test]
fn a_folder_that_cannot_be_written_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), b"").is_ok() {
        return; // Running as root: permissions are not enforced.
    }
    let message = rejection(
        &DaemonCore::new(),
        SetDefaultsParams {
            folder: Some(locked.clone()),
            ..Default::default()
        },
    );
    assert!(
        message.starts_with("folder: can't write there ("),
        "{message}"
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_missing_folder_inside_one_that_cannot_be_written_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), b"").is_ok() {
        return; // Running as root: permissions are not enforced.
    }
    let message = rejection(
        &DaemonCore::new(),
        SetDefaultsParams {
            folder: Some(locked.join("not made yet")),
            ..Default::default()
        },
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        message.starts_with("folder: can't write there ("),
        "{message}"
    );
}

#[test]
fn a_default_folder_inside_a_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let message = rejection(
        &DaemonCore::new(),
        SetDefaultsParams {
            folder: Some(file.join("sub")),
            ..Default::default()
        },
    );
    assert_eq!(message, "folder: is a file, not a folder");
}

fn add_to(
    core: &DaemonCore,
    destination: std::path::PathBuf,
) -> Result<DownloadSummary, rustdm::ipc::protocol::RpcError> {
    core.add_download(AddDownloadParams {
        url: "http://127.0.0.1:9/a.bin".into(),
        destination: Some(destination),
        filename: None,
        connections: None,
        max_bytes_per_sec: None,
    })
}

#[tokio::test]
async fn adding_into_a_file_or_a_relative_folder_is_refused_as_a_bad_path() {
    let core = DaemonCore::new();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();

    for bad in [file.clone(), file.join("sub"), "relative/dir".into()] {
        let err = add_to(&core, bad.clone()).expect_err("must be refused");
        assert_eq!(err.code, CODE_INVALID_PATH, "{bad:?}: {}", err.message);
    }
    assert!(core.list_downloads().is_empty(), "nothing was queued");
}

#[cfg(unix)]
#[tokio::test]
async fn adding_into_a_folder_that_cannot_be_written_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let core = DaemonCore::new();
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), b"").is_ok() {
        return; // Running as root: permissions are not enforced.
    }
    let result = add_to(&core, locked.join("new"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = result.expect_err("must be refused");
    assert_eq!(err.code, CODE_PERMISSION_DENIED, "{}", err.message);
}

#[tokio::test]
async fn adding_into_a_folder_that_does_not_exist_yet_is_allowed() {
    let core = DaemonCore::new();
    let dir = tempfile::tempdir().unwrap();
    add_to(&core, dir.path().join("made").join("later")).expect("queued");
}

#[test]
fn defaults_survive_a_daemon_restart() {
    let data = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let wanted = {
        let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();
        core.set_defaults(SetDefaultsParams {
            folder: Some(downloads.path().to_path_buf()),
            connections: Some(3),
            max_bytes_per_sec: Some(1024 * 1024),
        })
        .unwrap()
    };
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();
    assert_eq!(core.get_defaults(), wanted);
}

#[tokio::test]
async fn defaults_can_be_read_and_changed_over_ipc() {
    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind_with_core(&endpoint, DaemonCore::new())
        .await
        .unwrap();
    tokio::spawn(async move { server.run().await.unwrap() });
    let mut client = DaemonClient::connect(&endpoint).await.unwrap();

    assert_eq!(client.get_defaults().await.unwrap().connections, 8);
    let changed = client
        .set_defaults(SetDefaultsParams {
            connections: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(changed.connections, 2);
    let err = client
        .set_defaults(SetDefaultsParams {
            connections: Some(99),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "connections: must be between 1 and 16");
}

async fn throttled_server() -> TestServer {
    TestServer::start(
        pattern_bytes(1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 32 * 1024,
        },
    )
    .await
}

fn add_without_options(url: String) -> AddDownloadParams {
    AddDownloadParams {
        url,
        destination: None,
        filename: None,
        connections: None,
        max_bytes_per_sec: None,
    }
}

#[tokio::test]
async fn add_without_a_destination_uses_the_default_folder() {
    let mock = throttled_server().await;
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::new();
    core.set_defaults(SetDefaultsParams {
        folder: Some(folder.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();

    let summary = core
        .add_download(add_without_options(mock.url().to_string()))
        .unwrap();
    assert_eq!(summary.destination, folder.path());
    core.cancel_download(CancelDownloadParams {
        id: summary.id,
        delete_partial: true,
    })
    .unwrap();
}

#[tokio::test]
async fn add_takes_connections_and_speed_from_the_defaults() {
    let mock = throttled_server().await;
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::new();
    core.set_defaults(SetDefaultsParams {
        folder: Some(folder.path().to_path_buf()),
        connections: Some(3),
        max_bytes_per_sec: Some(64 * 1024),
    })
    .unwrap();

    let summary = core
        .add_download(add_without_options(mock.url().to_string()))
        .unwrap();
    assert_eq!(summary.max_bytes_per_sec, 64 * 1024);
    let connections = core.state.lock().unwrap().downloads[&summary.id]
        .lock()
        .unwrap()
        .connections;
    assert_eq!(connections, 3);
    core.cancel_download(CancelDownloadParams {
        id: summary.id,
        delete_partial: true,
    })
    .unwrap();
}

fn ping(core: &DaemonCore) -> PingResult {
    let reply = core.handle_request(RpcRequest::new(
        RpcId::Number(1),
        "daemon.ping".to_string(),
        None,
    ));
    serde_json::from_value(reply.result.expect("ping result")).unwrap()
}

#[test]
fn ping_reports_the_log_path_of_a_daemon_with_a_data_folder() {
    let data = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();
    let reply = ping(&core);
    assert_eq!(reply.data_dir.as_deref(), Some(data.path()));
    assert_eq!(reply.log_path, Some(data.path().join("rdmd.log")));
}

#[test]
fn ping_without_a_data_folder_reports_none() {
    let reply = ping(&DaemonCore::new());
    assert_eq!(reply.data_dir, None);
    assert_eq!(reply.log_path, None);
}
