use rustdm::engine::{build_client, resume, Outcome};
use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::AddDownloadParams;
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A per-test endpoint. On Unix the socket and its lock file live in `dir`.
fn test_endpoint(dir: &Path) -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    if cfg!(unix) {
        IpcEndpoint::UnixSocket(dir.join(format!("rdmd_{id}.sock")))
    } else {
        IpcEndpoint::test_endpoint(&format!("rdmd_{id}"))
    }
}

fn spawn_rdmd(endpoint: &IpcEndpoint) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rdmd"))
        .arg("--endpoint")
        .arg(endpoint.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rdmd")
}

async fn wait_for_daemon(endpoint: &IpcEndpoint) -> DaemonClient {
    let start = Instant::now();
    loop {
        match DaemonClient::connect(endpoint).await {
            Ok(client) => return client,
            Err(ClientError::NotRunning) if start.elapsed() < STARTUP_TIMEOUT => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("rdmd did not start: {e}"),
        }
    }
}

async fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    tokio::time::timeout(EXIT_TIMEOUT, child.wait())
        .await
        .expect("rdmd exits in time")
        .expect("wait on rdmd")
}

fn find_state_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".part.rdm"))
}

#[tokio::test]
async fn rdmd_refuses_to_start_a_second_instance() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = test_endpoint(dir.path());
    let mut first = spawn_rdmd(&endpoint);
    let mut client = wait_for_daemon(&endpoint).await;

    let second = spawn_rdmd(&endpoint);
    let output = tokio::time::timeout(EXIT_TIMEOUT, second.wait_with_output())
        .await
        .expect("second rdmd exits")
        .expect("wait on second rdmd");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "stderr was: {stderr}");

    // The first daemon keeps serving.
    client.ping().await.expect("first daemon still answers");
    client.shutdown().await.expect("shutdown");
    assert!(wait_for_exit(&mut first).await.success());
}

#[tokio::test]
async fn rdmd_shutdown_pauses_downloads_so_they_can_resume() {
    let body = pattern_bytes(4 * 1024 * 1024);
    let mock = TestServer::start(
        body.clone(),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let endpoint = test_endpoint(dir.path());
    let mut daemon = spawn_rdmd(&endpoint);
    let mut client = wait_for_daemon(&endpoint).await;

    let added = client
        .add(AddDownloadParams {
            url: mock.url().to_string(),
            destination: Some(downloads.path().to_path_buf()),
            filename: Some("interrupted.bin".to_string()),
            connections: Some(2),
        })
        .await
        .expect("add");

    let start = Instant::now();
    while client.status(&added.id).await.unwrap().downloaded_bytes == 0 {
        assert!(start.elapsed() < STARTUP_TIMEOUT, "download never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    client.shutdown().await.expect("shutdown");
    assert!(wait_for_exit(&mut daemon).await.success());
    if let Some(socket) = endpoint.as_path() {
        assert!(!socket.exists(), "socket file is removed on shutdown");
    }

    let state = find_state_file(downloads.path()).expect("state file saved on shutdown");
    match resume(&build_client(), &state, None).await.expect("resume") {
        Outcome::Completed { path, .. } => assert_eq!(std::fs::read(path).unwrap(), body),
        other => panic!("expected completion, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rdmd_exits_cleanly_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = test_endpoint(dir.path());
    let mut daemon = spawn_rdmd(&endpoint);
    let _client = wait_for_daemon(&endpoint).await;

    let pid = daemon.id().expect("rdmd pid") as libc::pid_t;
    // SAFETY: sending a signal to a child process we own.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);

    assert!(wait_for_exit(&mut daemon).await.success());
    assert!(!endpoint.as_path().unwrap().exists());
}
