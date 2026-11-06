use rustdm::daemon::DaemonServer;
use rustdm::ipc::protocol::{
    AddDownloadParams, DownloadIdParam, DownloadStatus, DownloadSummary, PingResult, RpcId,
    RpcMessage, RpcRequest, CODE_DOWNLOAD_NOT_FOUND,
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
