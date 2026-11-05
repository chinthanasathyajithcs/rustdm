use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::{DaemonCore, DaemonServer};
use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::{
    AddDownloadParams, DownloadStatus, DownloadSummary, SetLimitsParams, CODE_DOWNLOAD_NOT_FOUND,
    CODE_INVALID_PARAMS, CODE_OPERATION_IN_PROGRESS,
};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

fn next_test_endpoint() -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::test_endpoint(&format!("queue_{}", id))
}

async fn serve(core: DaemonCore) -> (DaemonClient, tokio::task::JoinHandle<()>) {
    let endpoint = next_test_endpoint();
    let server = DaemonServer::bind_with_core(&endpoint, core)
        .await
        .expect("bind daemon");
    let task = tokio::spawn(async move {
        server.run().await.unwrap();
    });
    let client = DaemonClient::connect(&endpoint).await.expect("connect");
    (client, task)
}

fn add_params(mock: &TestServer, name: &str, dir: &Path) -> AddDownloadParams {
    AddDownloadParams {
        url: mock.url().join(name).unwrap().to_string(),
        destination: Some(dir.to_path_buf()),
        filename: Some(name.to_string()),
        connections: Some(1),
    }
}

fn limits(max_active: Option<usize>, max_bytes_per_sec: Option<u64>) -> SetLimitsParams {
    SetLimitsParams {
        max_active,
        max_bytes_per_sec,
    }
}

async fn sorted_list(client: &mut DaemonClient) -> Vec<DownloadSummary> {
    let mut list = client.list().await.expect("list");
    list.sort_by_key(|d| d.id[3..].parse::<u64>().unwrap());
    list
}

async fn wait_until_all_completed(client: &mut DaemonClient) {
    let start = Instant::now();
    loop {
        let list = client.list().await.expect("list");
        if list.iter().all(|d| d.status == DownloadStatus::Completed) {
            return;
        }
        assert!(
            start.elapsed() < WAIT_TIMEOUT,
            "downloads did not finish: {list:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn expect_rpc_code<T: std::fmt::Debug>(result: Result<T, ClientError>, code: i32) {
    match result {
        Err(ClientError::Rpc(err)) => assert_eq!(err.code, code, "{err:?}"),
        other => panic!("expected RPC error {code}, got {other:?}"),
    }
}

#[tokio::test]
async fn queue_runs_at_most_max_active_downloads_in_order() {
    let body = pattern_bytes(256 * 1024);
    let mock = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 512 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _server) = serve(DaemonCore::new()).await;
    client.set_limits(limits(Some(1), None)).await.unwrap();

    for name in ["a.bin", "b.bin", "c.bin"] {
        client
            .add(add_params(&mock, name, dir.path()))
            .await
            .unwrap();
    }
    let statuses: Vec<_> = sorted_list(&mut client)
        .await
        .iter()
        .map(|d| d.status)
        .collect();
    assert_eq!(
        statuses,
        vec![
            DownloadStatus::Active,
            DownloadStatus::Queued,
            DownloadStatus::Queued
        ]
    );

    let mut finish_order = Vec::new();
    let start = Instant::now();
    while finish_order.len() < 3 {
        let list = sorted_list(&mut client).await;
        let active = list
            .iter()
            .filter(|d| d.status == DownloadStatus::Active)
            .count();
        assert!(active <= 1, "more than one active download: {list:?}");
        for d in &list {
            if d.status == DownloadStatus::Completed && !finish_order.contains(&d.id) {
                finish_order.push(d.id.clone());
            }
        }
        assert!(start.elapsed() < WAIT_TIMEOUT, "queue stalled: {list:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(finish_order, vec!["dl_1", "dl_2", "dl_3"]);
    for name in ["a.bin", "b.bin", "c.bin"] {
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), body);
    }
}

#[tokio::test]
async fn bandwidth_limit_caps_the_combined_speed_of_all_downloads() {
    let body = pattern_bytes(192 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _server) = serve(DaemonCore::new()).await;
    let set = client
        .set_limits(limits(None, Some(128 * 1024)))
        .await
        .unwrap();
    assert_eq!(set.max_bytes_per_sec, 128 * 1024);

    let start = Instant::now();
    client
        .add(add_params(&mock, "x.bin", dir.path()))
        .await
        .unwrap();
    client
        .add(add_params(&mock, "y.bin", dir.path()))
        .await
        .unwrap();
    wait_until_all_completed(&mut client).await;

    // 384 KiB at 128 KiB/s with a one-second burst needs at least two seconds.
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1800),
        "finished in {elapsed:?}"
    );
    assert_eq!(std::fs::read(dir.path().join("x.bin")).unwrap(), body);
    assert_eq!(std::fs::read(dir.path().join("y.bin")).unwrap(), body);
}

#[tokio::test]
async fn set_limits_rejects_values_out_of_range() {
    let (mut client, _server) = serve(DaemonCore::new()).await;

    expect_rpc_code(
        client.set_limits(limits(Some(0), None)).await,
        CODE_INVALID_PARAMS,
    );
    expect_rpc_code(
        client.set_limits(limits(None, Some(1))).await,
        CODE_INVALID_PARAMS,
    );

    let unlimited = client.set_limits(limits(Some(5), Some(0))).await.unwrap();
    assert_eq!(unlimited.max_active, 5);
    assert_eq!(unlimited.max_bytes_per_sec, 0);
    assert_eq!(client.get_limits().await.unwrap(), unlimited);
}

#[tokio::test]
async fn queued_downloads_can_be_paused_and_removed_but_running_ones_cannot_be_removed() {
    let mock = TestServer::start(
        pattern_bytes(1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 128 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _server) = serve(DaemonCore::new()).await;
    client.set_limits(limits(Some(1), None)).await.unwrap();

    let running = client
        .add(add_params(&mock, "r.bin", dir.path()))
        .await
        .unwrap();
    let queued = client
        .add(add_params(&mock, "q.bin", dir.path()))
        .await
        .unwrap();
    assert_eq!(queued.status, DownloadStatus::Queued);

    let paused = client.pause(&queued.id).await.unwrap();
    assert_eq!(paused.status, DownloadStatus::Paused);

    expect_rpc_code(client.remove(&running.id).await, CODE_OPERATION_IN_PROGRESS);
    client.remove(&queued.id).await.unwrap();
    expect_rpc_code(client.status(&queued.id).await, CODE_DOWNLOAD_NOT_FOUND);
    assert_eq!(client.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancel_with_delete_removes_partial_files() {
    let mock = TestServer::start(
        pattern_bytes(2 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _server) = serve(DaemonCore::new()).await;

    let added = client
        .add(add_params(&mock, "gone.bin", dir.path()))
        .await
        .unwrap();
    let start = Instant::now();
    while client.status(&added.id).await.unwrap().downloaded_bytes == 0 {
        assert!(start.elapsed() < WAIT_TIMEOUT, "download never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let cancelled = client.cancel(&added.id, true).await.unwrap();
    assert_eq!(cancelled.status, DownloadStatus::Cancelled);

    let start = Instant::now();
    while std::fs::read_dir(dir.path()).unwrap().next().is_some() {
        assert!(
            start.elapsed() < WAIT_TIMEOUT,
            "partial files left behind: {:?}",
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn queue_and_limits_survive_a_daemon_restart() {
    let body = pattern_bytes(1024 * 1024);
    let mock = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 512 * 1024,
        },
    )
    .await;
    let data = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // First daemon: one running, one queued, one paused by the user.
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();
    let (mut client, server) = serve(core.clone()).await;
    client.set_limits(limits(Some(1), None)).await.unwrap();
    let a = client
        .add(add_params(&mock, "a.bin", dir.path()))
        .await
        .unwrap();
    let b = client
        .add(add_params(&mock, "b.bin", dir.path()))
        .await
        .unwrap();
    let c = client
        .add(add_params(&mock, "c.bin", dir.path()))
        .await
        .unwrap();
    client.pause(&c.id).await.unwrap();

    let start = Instant::now();
    while client.status(&a.id).await.unwrap().downloaded_bytes == 0 {
        assert!(start.elapsed() < WAIT_TIMEOUT, "download never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client.shutdown().await.unwrap();
    server.await.unwrap();
    assert!(core.wait_until_settled(Duration::from_secs(10)).await);
    assert!(data.path().join("queue.json").exists());

    // Second daemon: interrupted and queued downloads come back queued, the paused one stays paused.
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();
    let restored: Vec<_> = {
        let mut list = core.list_downloads();
        list.sort_by_key(|d| d.id.clone());
        list.into_iter().map(|d| (d.id, d.status)).collect()
    };
    assert_eq!(
        restored,
        vec![
            (a.id.clone(), DownloadStatus::Queued),
            (b.id.clone(), DownloadStatus::Queued),
            (c.id.clone(), DownloadStatus::Paused),
        ]
    );

    let (mut client, _server) = serve(core).await;
    assert_eq!(client.get_limits().await.unwrap().max_active, 1);
    client.remove(&c.id).await.unwrap();
    wait_until_all_completed(&mut client).await;
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), body);
    assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), body);

    // New ids continue after the restored ones.
    let d = client
        .add(add_params(&mock, "d.bin", dir.path()))
        .await
        .unwrap();
    assert_eq!(d.id, "dl_4");
}
