//! `rdm-nmhost` end to end: the real host binary, a real `rdmd`, and a local server.

use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::{AddDownloadParams, AddedBy, DownloadStatus};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::nmhost::frame::{read_frame, write_frame};
use rustdm::nmhost::origin::ALLOWED_EXTENSION_IDS;
use rustdm::testing::{pattern_bytes, Mode, Route, TestServer};
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

const SIGNED_IN_BODY_BYTES: u64 = 2 * 1024 * 1024;
const COOKIE_VALUE: &str = "hush-hush";
const PAGE: &str = "https://files.example.com/page";
const BROWSER_AGENT: &str = "TestBrowser/1.0";

fn extension_cookie(value: &str) -> Value {
    json!({
        "name": "sid", "value": value, "domain": "127.0.0.1", "path": "/",
        "secure": false, "host_only": true, "expires": 4_102_444_800.5
    })
}

#[tokio::test]
async fn an_add_with_cookies_referer_and_user_agent_reaches_the_probe_and_every_worker() {
    let body = pattern_bytes(SIGNED_IN_BODY_BYTES);
    let server = TestServer::start(body.clone(), Mode::Normal).await;
    server.require_header("cookie", &format!("sid={COOKIE_VALUE}"));
    server.require_header("referer", PAGE);
    server.require_header("user-agent", BROWSER_AGENT);
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(json!({
            "type": "add", "id": 1, "url": server.url().to_string(), "filename": "signed.bin",
            "context": {
                "cookies": [extension_cookie(COOKIE_VALUE)],
                "referer": PAGE, "user_agent": BROWSER_AGENT
            }
        }))
        .await;
    assert_eq!(reply["ok"], true, "{reply}");
    let id = reply["result"]["download_id"].as_str().unwrap().to_string();
    wait_for_download(&mut client, &id, &env.downloads(), "signed.bin").await;
    assert_eq!(
        std::fs::read(env.downloads().join("signed.bin")).unwrap(),
        body
    );

    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|r| r.range.as_deref() == Some("bytes=0-0")),
        "the size probe was sent"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.range.as_deref().is_some_and(|range| range != "bytes=0-0")),
        "at least one worker was sent"
    );
    for request in &requests {
        let cookie = request.cookie.as_deref().unwrap_or_default();
        assert!(
            cookie.contains(&format!("sid={COOKIE_VALUE}")),
            "{request:?}"
        );
        assert_eq!(request.referer.as_deref(), Some(PAGE), "{request:?}");
        assert_eq!(
            request.user_agent.as_deref(),
            Some(BROWSER_AGENT),
            "{request:?}"
        );
    }

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn an_invalid_context_is_refused_with_a_fixed_message_and_nothing_is_queued() {
    let server = TestServer::start(pattern_bytes(32 * 1024), Mode::Normal).await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let too_many: Vec<Value> = (0..rustdm::request_context::MAX_COOKIES + 1)
        .map(|_| extension_cookie(COOKIE_VALUE))
        .collect();
    let reply = host
        .call(json!({
            "type": "add", "id": 1, "url": server.url().to_string(),
            "context": {"cookies": too_many}
        }))
        .await;

    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["error"]["kind"], "invalid");
    assert_eq!(
        reply["error"]["message"],
        format!(
            "Invalid params: cookies: more than {} cookies",
            rustdm::request_context::MAX_COOKIES
        )
    );
    assert!(!reply.to_string().contains(COOKIE_VALUE));
    assert!(client.list().await.unwrap().is_empty());
    assert!(server.requests().is_empty(), "nothing was fetched");

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn a_cookie_of_the_wrong_shape_is_refused_by_the_host_without_quoting_it() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(json!({
            "type": "add", "id": 4, "url": "https://example.com/x",
            "context": {"cookies": [{"value": COOKIE_VALUE, "domain": "example.com"}]}
        }))
        .await;

    assert_eq!(reply["ok"], false);
    assert_eq!(reply["id"], 4);
    assert_eq!(reply["error"]["kind"], "invalid");
    assert!(!reply.to_string().contains(COOKIE_VALUE), "{reply}");
    assert!(host.finish().await.success());
}

#[tokio::test]
async fn an_add_without_context_still_works_and_sends_no_cookies() {
    let body = pattern_bytes(64 * 1024);
    let server = TestServer::start(body.clone(), Mode::Normal).await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(json!({
            "type": "add", "id": 1, "url": server.url().to_string(), "filename": "plain.bin"
        }))
        .await;
    assert_eq!(reply["ok"], true, "{reply}");
    let id = reply["result"]["download_id"].as_str().unwrap().to_string();
    wait_for_download(&mut client, &id, &env.downloads(), "plain.bin").await;
    assert_eq!(
        std::fs::read(env.downloads().join("plain.bin")).unwrap(),
        body
    );
    for request in server.requests() {
        assert!(request.cookie.is_none(), "{request:?}");
        assert!(request.referer.is_none(), "{request:?}");
    }

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

const MASTER_PLAYLIST: &str = "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720
720/index.m3u8
";

fn slow_file_server() -> impl std::future::Future<Output = TestServer> {
    TestServer::start(
        pattern_bytes(2 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 64 * 1024,
        },
    )
}

/// Ask the host about `id` until it reports `want`.
async fn host_status_until(host: &mut Host, id: &str, want: &str) -> Value {
    let start = Instant::now();
    loop {
        let reply = host
            .call(json!({"type": "status", "id": 7, "download_id": id}))
            .await;
        if reply["result"]["status"] == want {
            return reply;
        }
        assert!(
            start.elapsed() < REPLY_TIMEOUT,
            "never got to {want}: {reply}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_host_reports_its_own_downloads_trimmed_and_nothing_else() {
    let server = slow_file_server().await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);
    let added = host
        .call(
            json!({"type": "add", "id": 1, "url": server.url().join("a.bin").unwrap().to_string()}),
        )
        .await;
    let id = added["result"]["download_id"].as_str().unwrap().to_string();

    let mine = host
        .call(json!({"type": "status", "id": 2, "download_id": id}))
        .await;
    assert_eq!(mine["ok"], true, "{mine}");
    assert_eq!(mine["id"], 2);
    assert_eq!(mine["result"]["download_id"], id.as_str());
    let text = mine.to_string();
    assert!(
        !text.contains("http") && !text.contains("Downloads"),
        "{text}"
    );

    let theirs = client
        .add(AddDownloadParams {
            url: server.url().join("b.bin").unwrap().to_string(),
            destination: None,
            filename: None,
            connections: None,
            max_bytes_per_sec: None,
            context: None,
            quality: None,
            added_by: None,
        })
        .await
        .unwrap();
    let other = host
        .call(json!({"type": "status", "id": 3, "download_id": theirs.id}))
        .await;
    let missing = host
        .call(json!({"type": "status", "id": 3, "download_id": "dl_999"}))
        .await;
    assert_eq!(other["error"]["kind"], "not_found", "{other}");
    assert_eq!(other, missing, "the browser cannot tell the two apart");
    let choose = host
        .call(json!({"type": "choose_quality", "id": 4, "download_id": theirs.id, "choice_id": "720"}))
        .await;
    assert_eq!(choose["error"]["kind"], "not_found", "{choose}");

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn status_without_a_daemon_is_not_running_and_starts_none() {
    let env = Env::new();
    let mut host = env.spawn_host(&[&allowed_origin()]);

    let reply = host
        .call(json!({"type": "status", "id": 1, "download_id": "dl_1"}))
        .await;
    let choose = host
        .call(json!({"type": "choose_quality", "id": 2, "download_id": "dl_1", "choice_id": "a"}))
        .await;

    assert_eq!(reply["error"]["kind"], "not_running", "{reply}");
    assert_eq!(choose["error"]["kind"], "not_running", "{choose}");
    assert!(host.finish().await.success());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(
        DaemonClient::connect(&env.endpoint).await,
        Err(ClientError::NotRunning)
    ));
}

#[tokio::test]
async fn a_video_the_browser_sent_is_chosen_through_the_host() {
    let server = TestServer::start_routes(vec![(
        "/v/master.m3u8".to_string(),
        Route::ok(MASTER_PLAYLIST).with_header("Content-Type", "application/vnd.apple.mpegurl"),
    )])
    .await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);
    let added = host
        .call(json!({
            "type": "add", "id": 1, "url": server.url().join("v/master.m3u8").unwrap().to_string(),
            "context": {"title": "Angel One"}
        }))
        .await;
    let id = added["result"]["download_id"].as_str().unwrap().to_string();

    let waiting = host_status_until(&mut host, &id, "choosing").await;
    let video = &waiting["result"]["video"];
    assert_eq!(video["title"], "Angel One");
    assert_eq!(video["choices"][0]["label"], "1080p");
    assert_eq!(video["choices"][0]["container"], "MP4");
    let choice = video["choices"][1]["id"].as_str().unwrap().to_string();

    let chosen = host
        .call(json!({"type": "choose_quality", "id": 5, "download_id": id, "choice_id": choice}))
        .await;
    assert_eq!(chosen["ok"], true, "{chosen}");
    assert_ne!(chosen["result"]["status"], "choosing");
    assert_eq!(chosen["result"]["video"]["chosen"]["label"], "720p");

    let again = host
        .call(json!({"type": "choose_quality", "id": 6, "download_id": id, "choice_id": choice}))
        .await;
    assert_eq!(again["error"]["kind"], "rejected", "{again}");

    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn a_duplicate_from_the_browser_names_the_existing_download() {
    let server = slow_file_server().await;
    let env = Env::new();
    let mut daemon = env.spawn_rdmd();
    let mut client = env.wait_for_daemon().await;
    let mut host = env.spawn_host(&[&allowed_origin()]);
    let add = json!({"type": "add", "id": 1, "url": server.url().to_string()});

    let first = host.call(add.clone()).await;
    let second = host.call(add).await;

    assert_eq!(second["error"]["kind"], "duplicate");
    assert_eq!(
        second["error"]["existing_id"],
        first["result"]["download_id"]
    );
    let listed = client.list().await.unwrap();
    assert_eq!(listed[0].added_by, Some(AddedBy::Browser));
    assert!(host.finish().await.success());
    client.shutdown().await.unwrap();
    let _ = daemon.wait().await;
}
