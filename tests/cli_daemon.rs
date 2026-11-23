use rustdm::daemon::queue_store::DATA_DIR_ENV;
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest one `rdm` command may take; a hang fails the test instead of stalling CI.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `rdm` against one endpoint and stops the daemon it started when dropped.
/// The daemon keeps its queue file in the test's own folder.
struct Rdm {
    endpoint: String,
    data_dir: PathBuf,
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
            data_dir: dir.join("data"),
        }
    }

    /// Run `rdm`, failing the test if it does not finish within `COMMAND_TIMEOUT`.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rdm"));
        cmd.env(DATA_DIR_ENV, &self.data_dir)
            .arg("--endpoint")
            .arg(&self.endpoint)
            .args(args);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        rx.recv_timeout(COMMAND_TIMEOUT)
            .unwrap_or_else(|_| panic!("rdm {args:?} did not finish within {COMMAND_TIMEOUT:?}"))
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

    /// Wait until the rows of `rdm list` (without its header) satisfy `done`.
    fn wait_for_rows(&self, done: impl Fn(&[&str]) -> bool) -> String {
        let start = Instant::now();
        loop {
            let listing = self.ok(&["list"]);
            let rows: Vec<&str> = listing.lines().skip(1).collect();
            if !rows.is_empty() && done(&rows) {
                return listing;
            }
            assert!(
                start.elapsed() < WAIT_TIMEOUT,
                "`rdm list` never got there; last output:\n{listing}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
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
async fn rdm_add_sends_the_referer_and_user_agent_it_was_given() {
    let body = pattern_bytes(256 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    mock.require_header("referer", "https://files.example.com/page");
    mock.require_header("user-agent", "CliTest/1.0");
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url = mock.url().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    tokio::task::block_in_place(|| {
        rdm.ok(&[
            "add",
            &url,
            "--dir",
            &dest,
            "--name",
            "ctx.bin",
            "--referer",
            "https://files.example.com/page",
            "--user-agent",
            "CliTest/1.0",
        ]);
        rdm.wait_for_list("completed");
    });

    assert_eq!(
        std::fs::read(downloads.path().join("ctx.bin")).unwrap(),
        body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_refuses_a_referer_that_is_not_a_web_address() {
    let dir = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());

    let out = tokio::task::block_in_place(|| {
        rdm.run(&[
            "add",
            "http://127.0.0.1:9/a.bin",
            "--referer",
            "ftp://example.com/",
        ])
    });

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("referer"));
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

#[tokio::test(flavor = "multi_thread")]
async fn rdm_limits_queue_extra_downloads_and_survive_a_restart() {
    let mock = TestServer::start(
        pattern_bytes(4 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url_a = mock.url().join("a.bin").unwrap().to_string();
    let url_b = mock.url().join("b.bin").unwrap().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    tokio::task::block_in_place(|| {
        rdm.ok(&["daemon", "start"]);
        let limits = rdm.ok(&["limits", "--max-active", "1", "--max-speed", "1M"]);
        assert!(limits.contains("Downloads at once: 1"), "{limits}");
        assert!(limits.contains("1.0 MiB/s"), "{limits}");

        assert!(rdm.ok(&["add", &url_a, "--dir", &dest]).contains("active"));
        assert!(rdm.ok(&["add", &url_b, "--dir", &dest]).contains("queued"));

        assert!(rdm.ok(&["daemon", "stop"]).contains("stopped"));
        rdm.ok(&["daemon", "start"]);

        let listing = rdm.wait_for_list("active");
        assert!(
            listing.contains("dl_1") && listing.contains("dl_2"),
            "{listing}"
        );
        assert!(listing.contains("queued"), "{listing}");
        assert!(rdm.ok(&["limits"]).contains("Downloads at once: 1"));

        assert!(rdm
            .ok(&["cancel", "dl_2", "--delete"])
            .contains("cancelled"));
        assert!(rdm.ok(&["remove", "dl_2"]).contains("Removed dl_2"));
        assert!(!rdm.ok(&["list"]).contains("dl_2"));
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_moves_and_caps_downloads_and_falls_back_to_plain_output() {
    let mock = TestServer::start(
        pattern_bytes(256 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let dest = downloads.path().to_string_lossy().into_owned();

    // Plain `rdm` with its output piped prints rows instead of taking over the screen.
    assert!(rdm.ok(&[]).contains("not running"));

    tokio::task::block_in_place(|| {
        for (i, name) in ["one.bin", "two.bin"].iter().enumerate() {
            let url = mock.url().join(name).unwrap().to_string();
            rdm.ok(&["add", &url, "--dir", &dest, "--max-speed", "1M"]);
            rdm.ok(&["pause", &format!("dl_{}", i + 1)]);
        }
        rdm.ok(&["limits", "--max-active", "1"]);
    });
    let listing = rdm.wait_for_rows(|rows| rows.iter().all(|r| r.contains("paused")));
    assert!(listing.contains("[cap 1.0 MiB/s]"), "{listing}");

    let moved = rdm.ok(&["move", "dl_2", "top"]);
    let first_row = moved.lines().nth(1).unwrap_or_default();
    assert!(first_row.starts_with("dl_2"), "move output:\n{moved}");

    let capped = rdm.ok(&["speed", "dl_2", "off"]);
    assert!(capped.contains("dl_2: speed cap unlimited"), "{capped}");
    let bad = rdm.run(&["speed", "dl_2", "1K"]);
    assert!(!bad.status.success());

    rdm.ok(&["resume", "dl_1"]);
    rdm.ok(&["resume", "dl_2"]);
    let plain = tokio::task::block_in_place(|| rdm.ok(&["tui", "--plain"]));
    // Plain mode prints a row each time it changes and exits once both are done.
    for id in ["dl_1", "dl_2"] {
        assert!(
            plain
                .lines()
                .any(|row| row.starts_with(id) && row.contains("completed")),
            "plain output:\n{plain}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_defaults_sets_and_prints() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let dest = downloads.path().to_string_lossy().into_owned();

    tokio::task::block_in_place(|| {
        rdm.ok(&["daemon", "start"]);
        assert!(rdm.ok(&["defaults"]).contains("Connections: 8"));
        let set = rdm.ok(&[
            "defaults",
            "--dir",
            &dest,
            "--connections",
            "3",
            "--speed",
            "1M",
        ]);
        assert!(set.contains(&format!("Folder: {dest}")), "{set}");
        assert!(set.contains("Connections: 3"), "{set}");
        assert!(set.contains("1.0 MiB/s"), "{set}");
        assert_eq!(rdm.ok(&["defaults"]), set);

        let bad = rdm.run(&["defaults", "--connections", "40"]);
        assert!(!bad.status.success());
        assert!(
            String::from_utf8_lossy(&bad.stderr).contains("connections: must be between 1 and 16")
        );
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_without_dir_follows_the_default_folder() {
    let body = pattern_bytes(256 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url = mock.url().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    tokio::task::block_in_place(|| {
        rdm.ok(&["daemon", "start"]);
        rdm.ok(&["defaults", "--dir", &dest]);
        let added = rdm.ok(&["add", &url, "--name", "follows.bin"]);
        assert!(added.contains(&dest), "{added}");
        rdm.wait_for_list("completed");
    });
    assert_eq!(
        std::fs::read(downloads.path().join("follows.bin")).unwrap(),
        body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_quality_is_passed_to_the_daemon() {
    let body = pattern_bytes(128 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());
    let url = mock.url().to_string();
    let dest = downloads.path().to_string_lossy().into_owned();

    // The daemon checks the value, so a bad one coming back as its error
    // shows that the flag reached it; a good one is accepted for a plain file.
    tokio::task::block_in_place(|| {
        let bad = rdm.run(&["add", &url, "--dir", &dest, "--quality", "hd"]);
        assert!(!bad.status.success());
        let added = rdm.ok(&[
            "add",
            &url,
            "--dir",
            &dest,
            "--name",
            "q.bin",
            "--quality",
            "1080p",
        ]);
        assert!(added.contains("dl_1"), "{added}");
        rdm.wait_for_list("completed");
    });
    assert_eq!(std::fs::read(downloads.path().join("q.bin")).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_rejects_a_bad_quality() {
    let dir = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());

    let out = tokio::task::block_in_place(|| {
        rdm.run(&["add", "http://127.0.0.1:9/x", "--quality", "hd"])
    });

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("quality must be best, audio or a height such as 1080p"),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rdm_add_help_explains_the_quality_flag() {
    let dir = tempfile::tempdir().unwrap();
    let rdm = Rdm::new(dir.path());

    let help = rdm.ok(&["add", "--help"]);

    assert!(
        help.contains("For a video page: best, audio, or a height such as 1080p (default: ask)"),
        "{help}"
    );
}
