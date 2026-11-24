//! A real YouTube video, end to end: the real tools manager fetches yt-dlp,
//! deno and ffmpeg, the engine downloads both streams, ffmpeg joins them.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{AddDownloadParams, DownloadStatus};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// "Rust in 100 Seconds", about 2.5 minutes long.
const VIDEO_URL: &str = "https://www.youtube.com/watch?v=5C_HPTJg5ek";
/// Tools, page lookup and both streams; generous for a slow line.
const DEADLINE: Duration = Duration::from_secs(15 * 60);

#[tokio::test]
#[ignore = "touches YouTube and GitHub and downloads the tools; run by hand before a release"]
async fn real_youtube_video_downloads() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();

    let added = core
        .add_download(AddDownloadParams {
            url: VIDEO_URL.to_string(),
            destination: Some(folder.path().to_path_buf()),
            filename: None,
            connections: None,
            max_bytes_per_sec: None,
            context: None,
            quality: Some("360p".to_string()),
        })
        .unwrap();

    let deadline = Instant::now() + DEADLINE;
    let done = loop {
        core.tick_telemetry();
        let row = core.get_status(&added.id).unwrap();
        match row.status {
            DownloadStatus::Completed => break row,
            DownloadStatus::Failed | DownloadStatus::Paused | DownloadStatus::Cancelled => {
                panic!("{:?}: {:?}", row.status, row.error_message)
            }
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "still {:?} {:?}",
            row.status,
            row.step
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    assert!(done.destination.is_file(), "{}", done.destination.display());
    if let Some(kinds) = stream_kinds(&done.destination) {
        assert_eq!(kinds, ["audio", "video"], "{}", done.destination.display());
    }
}

/// The kinds of the file's streams, sorted, as ffprobe reports them; none
/// when ffprobe is not installed.
fn stream_kinds(file: &Path) -> Option<Vec<String>> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type"])
        .args(["-of", "csv=p=0"])
        .arg(file)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "ffprobe failed on {}",
        file.display()
    );
    let mut kinds: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    kinds.sort();
    Some(kinds)
}
