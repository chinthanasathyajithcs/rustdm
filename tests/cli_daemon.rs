use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::path::Path;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Runs `rdm` against one endpoint and stops the daemon it started when dropped.
struct Rdm {
    endpoint: String,
}

impl Rdm {
    fn new(dir: &Path) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let endpoint = if cfg!(unix) {
            IpcEndpoint::UnixSocket(dir.join(format!("cli_{id}.sock")))
        } else {
            IpcEndpoint::test_endpoint(&format!("cli_{id}"))
        };
        Self {
            endpoint: endpoint.to_string(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_rdm"))
            .arg("--endpoint")
            .arg(&self.endpoint)
            .args(args)
            .output()
            .expect("run rdm")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "rdm {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn wait_for_list(&self, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let listing = self.ok(&["list"]);
            if listing.contains(needle) {
                return listing;
            }
            assert!(
                start.elapsed() < WAIT_TIMEOUT,
                "`rdm list` never showed {needle:?}; last output:\n{listing}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Rdm {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_reports_when_the_daemon_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());

    assert!(rdm.ok(&["daemon", "status"]).contains("not running"));
    assert!(rdm.ok(&["list"]).contains("not running"));
    assert!(rdm.ok(&["daemon", "stop"]).contains("not running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_starts_the_daemon_and_completes_the_download() {
    let body = pattern_bytes(512 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url = mock.url().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    let added =
        tokio::task::block_in_place(|| rdm.ok(&["add", &url, "--dir", &dest, "--name", "cli.bin"]));
    assert!(added.contains("dl_1"), "add output: {added}");

    let status = rdm.ok(&["daemon", "status"]);
    assert!(status.contains("is running"), "status output: {status}");

    let listing = tokio::task::block_in_place(|| rdm.wait_for_list("completed"));
    assert!(listing.contains("cli.bin"), "list output: {listing}");
    assert_eq!(
        std::fs::read(downloads.path().join("cli.bin")).unwrap(),
        body
    );

    let pause = rdm.run(&["pause", "dl_1"]);
    assert!(!pause.status.success());
    assert!(String::from_utf8_lossy(&pause.stderr).contains("already completed"));

    assert!(rdm.ok(&["daemon", "stop"]).contains("stopped"));
    assert!(rdm.ok(&["daemon", "status"]).contains("not running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_pauses_resumes_and_cancels_background_downloads() {
    let mock = TestServer::start(
        pattern_bytes(8 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url = mock.url().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    tokio::task::block_in_place(|| {
        rdm.ok(&["add", &url, "--dir", &dest, "--name", "slow.bin"]);
        rdm.wait_for_list("active");

        assert!(rdm.ok(&["pause", "dl_1"]).contains("dl_1"));
        rdm.wait_for_list("paused");

        assert!(rdm.ok(&["resume", "dl_1"]).contains("active"));
        rdm.wait_for_list("active");

        assert!(rdm.ok(&["cancel", "dl_1"]).contains("cancelled"));
        std::thread::sleep(Duration::from_millis(500));
        assert!(rdm.ok(&["list"]).contains("cancelled"));
    });
}
