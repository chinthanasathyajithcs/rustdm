//! `tools.status`, `tools.update` and `tools.rollback` through the daemon and its client.

use rustdm::daemon::{DaemonCore, DaemonServer};
use rustdm::engine::build_client;
use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::{ToolName, ToolPhase, CODE_TOOL_FAILED};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::fake_releases::*;
use rustdm::testing::TestServer;
use rustdm::tools::{FetchCtx, Freshness, Tool, ToolStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

const V1: &str = "2026.11.10.100000";
const V2: &str = "2026.11.14.200000";

struct Setup {
    client: DaemonClient,
    releases: TestServer,
    store: Arc<ToolStore>,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let endpoint = IpcEndpoint::test_endpoint(&format!("tools_rpc_{id}"));
    let dir = tempfile::tempdir().unwrap();
    let releases = TestServer::start_routes(vec![]).await;
    let store = Arc::new(
        ToolStore::with_sources(
            dir.path().join("tools"),
            build_client(),
            test_sources(&releases),
        )
        .unwrap(),
    );
    let core = DaemonCore::new().with_tools(store.clone());
    let server = DaemonServer::bind_with_core(&endpoint, core).await.unwrap();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let client = DaemonClient::connect(&endpoint).await.unwrap();
    Setup {
        client,
        releases,
        store,
        _dir: dir,
    }
}

#[tokio::test]
async fn status_lists_every_tool_as_not_installed_at_first() {
    let mut setup = setup().await;

    let status = setup.client.tools_status().await.unwrap();

    let names: Vec<ToolName> = status.iter().map(|i| i.tool).collect();
    assert_eq!(
        names,
        vec![ToolName::YtDlp, ToolName::Ffmpeg, ToolName::Deno]
    );
    for info in status {
        assert_eq!(info.installed, None);
        assert_eq!(info.previous, None);
        assert_eq!(info.phase, ToolPhase::Idle);
        assert_eq!(info.error, None);
    }
}

#[tokio::test]
async fn update_installs_a_missing_tool_in_the_background() {
    let mut setup = setup().await;
    publish_ytdlp(&setup.releases, V1, &fake_program(V1));

    let at_once = setup
        .client
        .tools_update(Some(ToolName::YtDlp))
        .await
        .unwrap();
    let ytdlp = at_once.iter().find(|i| i.tool == ToolName::YtDlp).unwrap();
    assert_ne!(
        ytdlp.phase,
        ToolPhase::Idle,
        "work has started when it answers"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = setup.client.tools_status().await.unwrap();
        let ytdlp = status.iter().find(|i| i.tool == ToolName::YtDlp).unwrap();
        if ytdlp.installed.as_deref() == Some(V1) && ytdlp.phase == ToolPhase::Idle {
            break;
        }
        assert!(Instant::now() < deadline, "never installed: {ytdlp:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn update_without_a_tool_checks_only_installed_tools() {
    let mut setup = setup().await;
    publish_ytdlp(&setup.releases, V1, &fake_program(V1));
    setup
        .store
        .ensure(Tool::YtDlp, Freshness::CheckNow, FetchCtx::new())
        .await
        .unwrap();
    publish_ytdlp(&setup.releases, V2, &fake_program(V2));

    setup.client.tools_update(None).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = setup.client.tools_status().await.unwrap();
        if status[0].installed.as_deref() == Some(V2) {
            assert_eq!(status[0].previous.as_deref(), Some(V1));
            // ffmpeg and deno were not installed, so they were left alone.
            assert_eq!(status[1].installed, None);
            assert_eq!(status[2].installed, None);
            assert_eq!(status[1].phase, ToolPhase::Idle);
            break;
        }
        assert!(Instant::now() < deadline, "never updated");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn rollback_swaps_versions_and_reports_the_result() {
    let mut setup = setup().await;
    publish_ytdlp(&setup.releases, V1, &fake_program(V1));
    setup
        .store
        .ensure(Tool::YtDlp, Freshness::CheckNow, FetchCtx::new())
        .await
        .unwrap();
    publish_ytdlp(&setup.releases, V2, &fake_program(V2));
    setup
        .store
        .ensure(Tool::YtDlp, Freshness::CheckNow, FetchCtx::new())
        .await
        .unwrap();

    let rolled = setup.client.tools_rollback(ToolName::YtDlp).await.unwrap();

    assert_eq!(rolled.tool, ToolName::YtDlp);
    assert_eq!(rolled.installed.as_deref(), Some(V1));
    assert_eq!(rolled.previous.as_deref(), Some(V2));
}

#[tokio::test]
async fn rollback_with_nothing_to_go_back_to_is_a_tool_error() {
    let mut setup = setup().await;

    match setup.client.tools_rollback(ToolName::Deno).await {
        Err(ClientError::Rpc(err)) => {
            assert_eq!(err.code, CODE_TOOL_FAILED);
            assert!(
                err.message.contains("no earlier version"),
                "{}",
                err.message
            );
        }
        other => panic!("expected a tool error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_in_memory_daemon_has_no_tools_folder() {
    let endpoint = IpcEndpoint::test_endpoint("tools_rpc_in_memory");
    let server = DaemonServer::bind(&endpoint).await.unwrap();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = DaemonClient::connect(&endpoint).await.unwrap();

    match client.tools_status().await {
        Err(ClientError::Rpc(err)) => assert_eq!(err.code, CODE_TOOL_FAILED),
        other => panic!("expected a tool error, got {other:?}"),
    }
}
