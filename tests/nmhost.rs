//! `rdm-nmhost` end to end: the real host binary, a real `rdmd`, and a local server.

use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::DownloadStatus;
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::nmhost::frame::{read_frame, write_frame};
use rustdm::nmhost::origin::ALLOWED_EXTENSION_IDS;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// A scratch area: the daemon endpoint, its data folder and a fake home whose
/// `Downloads` folder receives the files.
struct Env {
    dir: tempfile::TempDir,
    endpoint: IpcEndpoint,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home").join("Downloads")).unwrap();
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let endpoint = if cfg!(unix) {
            IpcEndpoint::UnixSocket(dir.path().join(format!("nm_{id}.sock")))
        } else {
            IpcEndpoint::test_endpoint(&format!("nm_{id}"))
        };
        Self { dir, endpoint }
    }

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("home").join("Downloads")
    }

    /// Settings shared by every program a test starts.
    fn configure(&self, command: &mut Command) {
        let home = self.dir.path().join("home");
        command
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUSTDM_DATA_DIR", self.dir.path().join("data"))
            .env("RUSTDM_ENDPOINT", self.endpoint.to_string())
            .kill_on_drop(true);
    }

    fn spawn_rdmd(&self) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdmd"));
        command
            .arg("--endpoint")
            .arg(self.endpoint.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.configure(&mut command);
        command.spawn().expect("spawn rdmd")
    }

    fn spawn_host(&self, args: &[&str]) -> Host {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdm-nmhost"));
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.configure(&mut command);
        let mut child = command.spawn().expect("spawn rdm-nmhost");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Host {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    async fn wait_for_daemon(&self) -> DaemonClient {
        let start = Instant::now();
        loop {
            match DaemonClient::connect(&self.endpoint).await {
                Ok(client) => return client,
                Err(ClientError::NotRunning) if start.elapsed() < STARTUP_TIMEOUT => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("rdmd did not start: {e}"),
            }
        }
    }
}

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

impl Host {
    async fn send_raw(&mut self, body: &[u8]) {
        write_frame(self.stdin.as_mut().unwrap(), body)
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        let body = tokio::time::timeout(REPLY_TIMEOUT, read_frame(&mut self.stdout))
            .await
            .expect("reply in time")
            .expect("readable frame")
            .expect("a reply, not end of input");
        serde_json::from_slice(&body).unwrap()
    }

    async fn call(&mut self, request: Value) -> Value {
        self.send_raw(request.to_string().as_bytes()).await;
        self.recv().await
    }

    async fn finish(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        tokio::time::timeout(REPLY_TIMEOUT, self.child.wait())
            .await
            .expect("host exits")
            .unwrap()
    }
}

fn allowed_origin() -> String {
    format!("chrome-extension://{}/", ALLOWED_EXTENSION_IDS[0])
}

async fn wait_for_download(client: &mut DaemonClient, id: &str, folder: &Path, name: &str) {
    let start = Instant::now();
    loop {
        let status = client.status(id).await.expect("status");
        if status.status == DownloadStatus::Completed {
            assert!(folder.join(name).exists(), "{name} is in the folder");
            return;
        }
        assert!(start.elapsed() < STARTUP_TIMEOUT, "download did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn ping_reports_the_daemon_without_starting_it() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host.call(json!({"type": "ping", "id": 1})).await;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["protocol"], 1);
    assert_eq!(reply["result"]["daemon"]["running"], false);
    assert!(
        matches!(
            DaemonClient::connect(&env.endpoint).await,
            Err(ClientError::NotRunning)
        ),
        "ping must not start rdmd"
    );

    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let reply = host.call(json!({"type": "ping", "id": 2})).await;
    assert_eq!(reply["result"]["daemon"]["running"], true);
    assert_eq!(reply["result"]["daemon"]["version"], rustdm::VERSION);

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn add_reaches_a_running_daemon_and_the_file_arrives() {
    let body = pattern_bytes(64 * 1024);
    let server = TestServer::start(body.clone(), Mode::Normal).await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(json!({
            "type": "add", "id": 2, "url": server.url().to_string(), "filename": "got.bin",
            "context": {"referer": "https://example.com/page", "user_agent": "test-agent",
                "cookies": []}
        }))
        .await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["id"], 2);
    assert_eq!(reply["result"]["filename"], "got.bin");
    let id = reply["result"]["download_id"].as_str().unwrap().to_string();

    let listed = client.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    wait_for_download(&mut client, &id, &env.downloads(), "got.bin").await;
    assert_eq!(
        std::fs::read(env.downloads().join("got.bin")).unwrap(),
        body
    );

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn add_starts_the_daemon_when_none_is_running() {
    let server = TestServer::start(pattern_bytes(32 * 1024), Mode::Normal).await;
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(
            json!({"type": "add", "id": 1, "url": server.url().to_string(),
            "filename": "started.bin"}),
        )
        .await;
    assert_eq!(reply["ok"], true, "{reply}");

    let mut client = env.wait_for_daemon().await;
    let id = reply["result"]["download_id"].as_str().unwrap().to_string();
    wait_for_download(&mut client, &id, &env.downloads(), "started.bin").await;

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn adding_the_same_link_twice_reports_a_duplicate() {
    let server = TestServer::start(
        pattern_bytes(2 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 64 * 1024,
        },
    )
    .await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let add = json!({"type": "add", "id": 1, "url": server.url().to_string(), "filename": "d.bin"});
    assert_eq!(host.call(add.clone()).await["ok"], true);
    let second = host.call(add).await;
    assert_eq!(second["ok"], false);
    assert_eq!(second["error"]["kind"], "duplicate");
    assert_eq!(second["error"]["code"], -32005);

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn a_wrong_origin_exits_without_reading_input() {
    let env = Env::new();
    let other = format!("chrome-extension://{}/", "a".repeat(32));
    for args in [vec![other.as_str()], vec![]] {
        let mut host = env.spawn_host(&args);
        // Never write a frame: the host must decide from its arguments alone.
        let status = tokio::time::timeout(REPLY_TIMEOUT, host.child.wait())
            .await
            .expect("host exits on its own")
            .unwrap();
        assert!(!status.success());
    }
}

#[tokio::test]
async fn the_parent_window_argument_does_not_get_in_the_way() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin(), "--parent-window=0"]);
    let reply = host.call(json!({"type": "ping", "id": 1})).await;
    assert_eq!(reply["ok"], true);
    assert!(host.finish().await.success());
}

#[tokio::test]
async fn frame_lengths_with_line_break_bytes_pass() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);

    // 13 bytes: CR in the length prefix. Not a valid request, so it is refused,
    // but the next frame proves the stream stayed in step.
    let thirteen = br#"{"type":"x"} "#;
    assert_eq!(thirteen.len(), 13);
    host.send_raw(thirteen).await;
    assert_eq!(host.recv().await["error"]["kind"], "invalid");

    // 266 bytes: LF in the second prefix byte. A ping padded with spaces.
    let mut padded = br#"{"type":"ping","id":9}"#.to_vec();
    padded.resize(266, b' ');
    host.send_raw(&padded).await;
    let reply = host.recv().await;
    assert_eq!(reply["id"], 9);
    assert_eq!(reply["ok"], true);

    assert!(host.finish().await.success());
}

#[tokio::test]
async fn a_shutdown_message_is_refused_and_the_daemon_stays_up() {
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    for message in [
        json!({"type": "shutdown", "id": 1}),
        json!({"type": "daemon.shutdown", "id": 2}),
        json!({"type": "add", "id": 3, "url": "https://example.com/x", "destination": "/tmp"}),
    ] {
        let reply = host.call(message).await;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["kind"], "invalid");
    }
    client.ping().await.expect("rdmd is still running");

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn an_oversized_frame_gets_a_too_large_reply_and_the_host_exits() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);
    use tokio::io::AsyncWriteExt;
    host.stdin
        .as_mut()
        .unwrap()
        .write_all(&(2u32 * 1024 * 1024).to_ne_bytes())
        .await
        .unwrap();
    let reply = host.recv().await;
    assert_eq!(reply["error"]["kind"], "too_large");
    assert!(host.finish().await.success());
}
