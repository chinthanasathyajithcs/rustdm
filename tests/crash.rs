use rustdm::testing::{pattern_bytes, Mode, TestServer};
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn crash_then_resume_is_byte_identical() {
    let runs: u32 = std::env::var("RDM_CRASH_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let rt = tokio::runtime::Runtime::new().unwrap();

    for run in 0..runs {
        let size = 8 * 1024 * 1024;
        let body = pattern_bytes(size);
        let srv = rt.block_on(async {
            TestServer::start(
                body.clone(),
                Mode::Throttle {
                    bytes_per_sec: 1024 * 1024,
                },
            )
            .await
        });

        let dir = tempdir().unwrap();
        let exe = env!("CARGO_BIN_EXE_rdm");

        let mut child = Command::new(exe)
            .arg("download")
            .arg(srv.url().join("crash.bin").unwrap().as_str())
            .arg("--out")
            .arg(dir.path().join("{name}").to_str().unwrap())
            .arg("--connections")
            .arg("4")
            .spawn()
            .unwrap();

        let wait_ms = fastrand::u64(300..=1500);
        std::thread::sleep(Duration::from_millis(wait_ms));

        let _ = child.kill();
        let _ = child.wait();

        let state_file = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with(".part.rdm"))
            .map(|e| e.path());

        if let Some(state_path) = state_file {
            let status = Command::new(exe)
                .arg("resume")
                .arg(&state_path)
                .status()
                .unwrap();
            assert!(status.success(), "resume failed in run {run}");

            let final_file = dir.path().join("crash.bin");
            assert!(final_file.exists());
            assert_eq!(std::fs::read(final_file).unwrap(), body);
        }
    }
}
