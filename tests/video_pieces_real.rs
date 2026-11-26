//! Real pieces videos, end to end: Apple's HLS sample as a direct link (fMP4
//! slices of one file, a separate sound track) and Akamai's DASH sample on a
//! page yt-dlp reads. Both must end as one file with video and sound.

use rustdm::daemon::queue_store::QueueStore;
use rustdm::daemon::DaemonCore;
use rustdm::ipc::protocol::{AddDownloadParams, DownloadStatus};
use rustdm::testing::{Route, TestServer};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const HLS_LINK: &str =
    "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8";
const DASH_MANIFEST: &str = "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd";
/// Tools, the playlist or lookup, and every piece; generous for a slow line.
const DEADLINE: Duration = Duration::from_secs(20 * 60);

#[tokio::test]
#[ignore = "touches Apple's and GitHub's servers and downloads the tools; run by hand before a release"]
async fn real_hls_direct_link_downloads() {
    downloads_with_video_and_sound(HLS_LINK).await;
}

#[tokio::test]
#[ignore = "touches Akamai's and GitHub's servers and downloads the tools; run by hand before a release"]
async fn real_dash_page_downloads() {
    // A raw .mpd link is out of scope (it saves as a file); a page that plays
    // it is what yt-dlp reads, as on a real site.
    let html = format!(
        "<html><body><video controls>\
         <source src=\"{DASH_MANIFEST}\" type=\"application/dash+xml\">\
         </video></body></html>"
    );
    let page = Route::ok(html).with_header("Content-Type", "text/html");
    let server = TestServer::start_routes(vec![("/bbb".to_string(), page)]).await;
    let link = server.url().join("/bbb").unwrap();

    downloads_with_video_and_sound(link.as_str()).await;
}

async fn downloads_with_video_and_sound(link: &str) {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let core = DaemonCore::with_store(QueueStore::in_dir(data.path())).unwrap();

    let added = core
        .add_download(AddDownloadParams {
            url: link.to_string(),
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
