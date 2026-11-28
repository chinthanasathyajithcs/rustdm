use rustdm::daemon::DaemonServer;
use rustdm::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use rustdm::ipc::protocol::{AddDownloadParams, DownloadStatus, CODE_DOWNLOAD_NOT_FOUND};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_test_endpoint() -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::test_endpoint(&format!("client_{}", id))
}

async fn start_daemon(endpoint: &IpcEndpoint) -> tokio::task::JoinHandle<()> {
    let server = DaemonServer::bind(endpoint).await.expect("bind daemon");
    tokio::spawn(async move {
        server.run().await.unwrap();
    })
}

#[tokio::test]
async fn client_pings_lists_and_shuts_down() {
    let endpoint = next_test_endpoint();
    let server_task = start_daemon(&endpoint).await;

    let mut client = DaemonClient::connect(&endpoint).await.expect("connect");
    let ping = client.ping().await.expect("ping");
    assert_eq!(ping.version, rustdm::VERSION);
    assert!(client.list().await.expect("list").is_empty());

    client.shutdown().await.expect("shutdown");
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .expect("server stops after shutdown")
        .unwrap();
}

#[tokio::test]
async fn client_reports_rpc_errors_with_their_code() {
    let endpoint = next_test_endpoint();
    let _server_task = start_daemon(&endpoint).await;

    let mut client = DaemonClient::connect(&endpoint).await.expect("connect");
    match client.pause("dl_missing").await {
        Err(ClientError::Rpc(err)) => assert_eq!(err.code, CODE_DOWNLOAD_NOT_FOUND),
        other => panic!("expected RPC error, got {other:?}"),
    }

    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn client_skips_telemetry_between_responses() {
    let mock = TestServer::start(
        pattern_bytes(2 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 512 * 1024,
        },
    )
    .await;
    let endpoint = next_test_endpoint();
    let _server_task = start_daemon(&endpoint).await;
    let tmp_dir = tempfile::tempdir().unwrap();

    let mut client = DaemonClient::connect(&endpoint).await.expect("connect");
    client.subscribe_telemetry().await.expect("subscribe");
    let added = client
        .add(AddDownloadParams {
            url: mock.url().to_string(),
            destination: Some(tmp_dir.path().to_path_buf()),
            filename: Some("ticks.bin".to_string()),
            connections: Some(2),
            max_bytes_per_sec: None,
            context: None,
            quality: None,
            added_by: None,
            choice_id: None,
            page: None,
        })
        .await
        .expect("add");

    // Telemetry ticks arrive every 250ms; every call must still get its own response.
    for _ in 0..8 {
        let status = client.status(&added.id).await.expect("status");
        assert_eq!(status.id, added.id);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn connect_reports_not_running_when_no_daemon_listens() {
    let endpoint = next_test_endpoint();
    match DaemonClient::connect(&endpoint).await {
        Err(ClientError::NotRunning) => {}
        other => panic!("expected NotRunning, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn connect_or_spawn_starts_the_daemon_when_missing() {
    let endpoint = next_test_endpoint();
    let spawn_endpoint = endpoint.clone();

    let mut client = connect_or_spawn(
        &endpoint,
        move || {
            tokio::spawn(async move {
                let server = DaemonServer::bind(&spawn_endpoint).await.unwrap();
                server.run().await.unwrap();
            });
            Ok(())
        },
        Duration::from_secs(5),
    )
    .await
    .expect("daemon started");

    client.ping().await.expect("ping");
    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn connect_or_spawn_reuses_a_running_daemon() {
    let endpoint = next_test_endpoint();
    let _server_task = start_daemon(&endpoint).await;
    let spawned = Arc::new(AtomicBool::new(false));
    let spawned_flag = Arc::clone(&spawned);

    let mut client = connect_or_spawn(
        &endpoint,
        move || {
            spawned_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        Duration::from_secs(5),
    )
    .await
    .expect("connect");

    assert!(
        !spawned.load(Ordering::SeqCst),
        "must not spawn a second daemon"
    );
    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn connect_or_spawn_times_out_when_the_daemon_never_listens() {
    let endpoint = next_test_endpoint();
    let result = connect_or_spawn(&endpoint, || Ok(()), Duration::from_millis(300)).await;
    match result {
        Err(ClientError::SpawnTimeout(_)) => {}
        other => panic!("expected SpawnTimeout, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn cancelled_download_stays_cancelled_after_the_engine_stops() {
    let mock = TestServer::start(
        pattern_bytes(4 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let endpoint = next_test_endpoint();
    let _server_task = start_daemon(&endpoint).await;
    let tmp_dir = tempfile::tempdir().unwrap();

    let mut client = DaemonClient::connect(&endpoint).await.expect("connect");
    let added = client
        .add(AddDownloadParams {
            url: mock.url().to_string(),
            destination: Some(tmp_dir.path().to_path_buf()),
            filename: Some("cancel_me.bin".to_string()),
            connections: Some(2),
            max_bytes_per_sec: None,
            context: None,
            quality: None,
            added_by: None,
            choice_id: None,
            page: None,
        })
        .await
        .expect("add");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cancelled = client.cancel(&added.id, false).await.expect("cancel");
    assert_eq!(cancelled.status, DownloadStatus::Cancelled);

    // The engine task finishes after the cancel signal; it must not overwrite the status.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = client.status(&added.id).await.expect("status");
    assert_eq!(status.status, DownloadStatus::Cancelled);

    client.shutdown().await.expect("shutdown");
}
