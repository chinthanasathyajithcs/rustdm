//! `rdm tools`: once against the real binaries (nothing installed, so no
//! network), and the update flow in-process against a pretend release host.

use rustdm::daemon::queue_store::DATA_DIR_ENV;
use rustdm::daemon::{DaemonCore, DaemonServer};
use rustdm::engine::build_client;
use rustdm::ipc::client::DaemonClient;
use rustdm::ipc::protocol::ToolName;
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::fake_releases::*;
use rustdm::testing::TestServer;
use rustdm::tools::cli::{run, ToolsCommand};
use rustdm::tools::ToolStore;
use std::path::Path;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const V1: &str = "2026.11.10.100000";
const V2: &str = "2026.11.14.200000";
const POLL: Duration = Duration::from_millis(20);

fn endpoint(dir: &Path, name: &str) -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    if cfg!(unix) {
        IpcEndpoint::UnixSocket(dir.join(format!("{name}_{id}.sock")))
    } else {
        IpcEndpoint::test_endpoint(&format!("{name}_{id}"))
    }
}

fn rdm(endpoint: &IpcEndpoint, data_dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_rdm"))
        .env(DATA_DIR_ENV, data_dir)
        .arg("--endpoint")
        .arg(endpoint.to_string())
        .args(args)
        .output()
        .expect("run rdm")
}

#[test]
fn the_binary_shows_tools_and_reports_problems_without_touching_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint(dir.path(), "tools_bin");
    let data = dir.path().join("data");

    let status = rdm(&endpoint, &data, &["tools"]);
    let text = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(text.starts_with("TOOL"), "{text}");
    for name in ["yt-dlp", "ffmpeg", "deno"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("not installed"), "{text}");

    let update = rdm(&endpoint, &data, &["tools", "update"]);
    assert!(update.status.success());
    assert!(String::from_utf8_lossy(&update.stdout).contains("No tools are installed yet"));

    let rollback = rdm(&endpoint, &data, &["tools", "rollback", "deno"]);
    assert!(!rollback.status.success());
    assert!(String::from_utf8_lossy(&rollback.stderr).contains("no earlier version"));

    let bad_name = rdm(&endpoint, &data, &["tools", "rollback", "wget"]);
    assert!(!bad_name.status.success());

    rdm(&endpoint, &data, &["daemon", "stop"]);
}

struct Setup {
    client: DaemonClient,
    releases: TestServer,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint(dir.path(), "tools_cli");
    let releases = TestServer::start_routes(vec![]).await;
    let store = Arc::new(
        ToolStore::with_sources(
            dir.path().join("tools"),
            build_client(),
            test_sources(&releases),
        )
        .unwrap(),
    );
    let server = DaemonServer::bind_with_core(&endpoint, DaemonCore::new().with_tools(store))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    Setup {
        client: DaemonClient::connect(&endpoint).await.unwrap(),
        releases,
        _dir: dir,
    }
}

async fn run_to_string(
    client: &mut DaemonClient,
    command: ToolsCommand,
) -> (Result<(), String>, String) {
    let mut out = Vec::new();
    let result = run(client, command, &mut out, POLL)
        .await
        .map_err(|e| e.to_string());
    (result, String::from_utf8(out).unwrap())
}

#[tokio::test]
async fn update_waits_for_the_install_and_prints_the_result() {
    let mut setup = setup().await;
    publish_ytdlp(&setup.releases, V1, &fake_program(V1));

    let (result, text) = run_to_string(
        &mut setup.client,
        ToolsCommand::Update(Some(ToolName::YtDlp)),
    )
    .await;

    assert_eq!(result, Ok(()));
    let last_table = text.lines().rfind(|l| l.starts_with("yt-dlp")).unwrap();
    assert!(last_table.contains(V1), "{text}");
    assert!(last_table.ends_with("installed"), "{text}");
}

#[tokio::test]
async fn update_fails_when_the_tool_could_not_be_fetched() {
    let mut setup = setup().await;
    // No release is published, so the lookup finds nothing.
    let (result, text) = run_to_string(
        &mut setup.client,
        ToolsCommand::Update(Some(ToolName::YtDlp)),
    )
    .await;

    assert_eq!(result, Err("could not update yt-dlp".to_string()));
    assert!(
        text.contains("GitHub refused the request for yt-dlp (HTTP 404)"),
        "{text}"
    );
}

#[tokio::test]
async fn rollback_prints_the_versions() {
    let mut setup = setup().await;
    publish_ytdlp(&setup.releases, V1, &fake_program(V1));
    let (first, _) = run_to_string(
        &mut setup.client,
        ToolsCommand::Update(Some(ToolName::YtDlp)),
    )
    .await;
    assert_eq!(first, Ok(()));
    publish_ytdlp(&setup.releases, V2, &fake_program(V2));
    let (second, _) = run_to_string(&mut setup.client, ToolsCommand::Update(None)).await;
    assert_eq!(second, Ok(()));

    let (result, text) =
        run_to_string(&mut setup.client, ToolsCommand::Rollback(ToolName::YtDlp)).await;

    assert_eq!(result, Ok(()));
    assert_eq!(text.trim(), format!("yt-dlp: now {V1} (was {V2})"));
}

#[tokio::test]
async fn status_prints_the_table() {
    let mut setup = setup().await;
    let (result, text) = run_to_string(&mut setup.client, ToolsCommand::Status).await;
    assert_eq!(result, Ok(()));
    assert_eq!(text.lines().count(), 4, "{text}");
}
