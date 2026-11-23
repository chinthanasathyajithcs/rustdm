//! Test helpers for the video features: a stand-in for yt-dlp (and deno) that
//! plays scripted answers, a pretend yt-dlp `-J` page, and a world that has
//! installed the stand-in through the real tools manager.
//!
//! The stand-in is the `rdm-test-tool` program; its code is in `tool_program`.

use super::fake_releases::{publish_ytdlp, test_sources, zip_bytes_stored, zip_pin};
use super::tool_program::{
    exe_dir, ARGS_FILE, COUNT_FILE, SCRIPT_FILE, SCRIPT_MARKER, VERSION_MARKER,
};
pub use super::tool_program::{run_test_tool, CookiesSeen, ToolResponse, ToolScript};
use super::{Route, TestServer};
use crate::engine::build_client;
use crate::tools::{FetchCtx, Freshness, Installed, Tool, ToolStore};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use url::Url;

/// The yt-dlp version `VideoWorld` installs.
pub const WORLD_YTDLP_VERSION: &str = "2026.11.10";
/// The deno version `VideoWorld` installs.
pub const WORLD_DENO_VERSION: &str = "2.9.7";

/// Make the stand-in installed at `exe` play `script`, starting from its first answer.
pub fn write_script(exe: &Path, script: &ToolScript) {
    let dir = exe_dir(exe);
    let json = serde_json::to_vec(script).expect("a script serializes");
    std::fs::write(dir.join(SCRIPT_FILE), json).expect("write the test tool script");
    let _ = std::fs::remove_file(dir.join(COUNT_FILE));
    let _ = std::fs::remove_file(dir.join(ARGS_FILE));
}

/// The arguments of every call the stand-in at `exe` has answered, in order.
/// A `--version` call is answered without being recorded.
pub fn recorded_args(exe: &Path) -> Vec<Vec<String>> {
    log_lines(exe)
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The cookies files the stand-in at `exe` found while it ran, in order.
pub fn recorded_cookies(exe: &Path) -> Vec<CookiesSeen> {
    log_lines(exe)
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn log_lines(exe: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(exe_dir(exe).join(ARGS_FILE)).unwrap_or_default();
    text.lines().map(String::from).collect()
}

/// The bytes of the stand-in program with `version` appended, so it can be
/// installed as yt-dlp or deno and answer `--version` with that version.
pub fn test_tool_program(version: &str) -> Vec<u8> {
    program_with_tail(version, None)
}

/// Like `test_tool_program`, with `script` carried inside the program: whoever
/// installs it gets a stand-in that plays `script` from its first call, before
/// any script file is written next to it. For a yt-dlp the daemon installs itself.
pub fn test_tool_program_scripted(version: &str, script: &ToolScript) -> Vec<u8> {
    program_with_tail(version, Some(script))
}

fn program_with_tail(version: &str, script: Option<&ToolScript>) -> Vec<u8> {
    let path = std::env::var_os("CARGO_BIN_EXE_rdm-test-tool")
        .expect("CARGO_BIN_EXE_rdm-test-tool is set when integration tests run");
    let mut bytes = std::fs::read(path).expect("read rdm-test-tool");
    if let Some(script) = script {
        bytes.extend_from_slice(SCRIPT_MARKER);
        bytes.extend_from_slice(&serde_json::to_vec(script).expect("a script serializes"));
    }
    bytes.extend_from_slice(VERSION_MARKER);
    bytes.extend_from_slice(version.as_bytes());
    bytes.push(0);
    bytes
}

/// A tools manager that has installed the stand-in as yt-dlp and as deno,
/// through a pretend release host, the way a first video lookup would.
pub struct VideoWorld {
    pub server: TestServer,
    pub store: Arc<ToolStore>,
    pub ytdlp: Installed,
    pub deno: Installed,
    _dir: TempDir,
}

impl VideoWorld {
    pub async fn new() -> VideoWorld {
        let server = TestServer::start_routes(vec![]).await;
        publish_ytdlp(
            &server,
            WORLD_YTDLP_VERSION,
            &test_tool_program(WORLD_YTDLP_VERSION),
        );
        let deno_name = if cfg!(windows) { "deno.exe" } else { "deno" };
        let archive = zip_bytes_stored(&[(deno_name, test_tool_program(WORLD_DENO_VERSION))]);
        let pin = zip_pin(&archive, WORLD_DENO_VERSION, 0, &[deno_name], deno_name, "");
        server.set_route(&format!("/{}", pin.path), Route::ok(archive));
        let mut sources = test_sources(&server);
        sources.pins.insert(Tool::Deno, pin);

        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = Arc::new(
            ToolStore::with_sources(dir.path().join("tools"), build_client(), sources)
                .expect("open the tools store"),
        );
        let ytdlp = ensure(&store, Tool::YtDlp).await;
        let deno = ensure(&store, Tool::Deno).await;
        VideoWorld {
            server,
            store,
            ytdlp,
            deno,
            _dir: dir,
        }
    }
}

async fn ensure(store: &ToolStore, tool: Tool) -> Installed {
    store
        .ensure(tool, Freshness::CheckIfStale, FetchCtx::new())
        .await
        .unwrap_or_else(|e| panic!("install the test {}: {e}", tool.name()))
}

/// One format of a pretend video page, in the shape yt-dlp lists it.
#[derive(Clone, Debug)]
pub struct FormatSpec {
    pub format_id: String,
    pub protocol: String,
    pub ext: String,
    /// `"none"` for an audio-only format, as yt-dlp says it.
    pub vcodec: String,
    /// `"none"` for a video-only format, as yt-dlp says it.
    pub acodec: String,
    pub height: Option<u32>,
    pub tbr: Option<f64>,
    pub filesize: Option<u64>,
    pub filesize_approx: Option<u64>,
    pub language_preference: Option<i32>,
    pub format_note: Option<String>,
    pub has_drm: bool,
    /// Becomes `downloader_options.http_chunk_size`, which YouTube needs.
    pub http_chunk_size: Option<u64>,
    pub url: String,
}

/// A pretend video (or playlist) page: what `yt-dlp -J` would print.
#[derive(Clone, Debug)]
pub struct PageSpec {
    pub title: String,
    pub extractor_key: String,
    pub duration: u64,
    pub is_live: bool,
    /// `Some` makes the page a playlist of that many entries.
    pub playlist_count: Option<u32>,
    pub formats: Vec<FormatSpec>,
}

fn format_json(format: &FormatSpec) -> serde_json::Value {
    let mut json = serde_json::json!({
        "format_id": format.format_id,
        "protocol": format.protocol,
        "url": format.url,
        "ext": format.ext,
        "vcodec": format.vcodec,
        "acodec": format.acodec,
        "height": format.height,
        "tbr": format.tbr,
        "filesize": format.filesize,
        "filesize_approx": format.filesize_approx,
        "language_preference": format.language_preference,
        "format_note": format.format_note,
        "has_drm": format.has_drm,
        "http_headers": {
            "User-Agent": "Mozilla/5.0 (test) rustdm",
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": "en-us,en;q=0.5",
            "Sec-Fetch-Mode": "navigate",
        },
    });
    if let Some(chunk) = format.http_chunk_size {
        json["downloader_options"] = serde_json::json!({ "http_chunk_size": chunk });
    }
    json
}

/// The `yt-dlp -J` output for `spec`.
pub fn video_page_json(spec: &PageSpec) -> String {
    let is_playlist = spec.playlist_count.is_some();
    let mut json = serde_json::json!({
        "id": "testVideo01",
        "title": spec.title,
        "extractor_key": spec.extractor_key,
        "duration": spec.duration,
        "is_live": spec.is_live,
        "webpage_url": "https://video.test/watch?v=testVideo01",
        "_type": if is_playlist { "playlist" } else { "video" },
        "formats": spec.formats.iter().map(format_json).collect::<Vec<_>>(),
    });
    if let Some(count) = spec.playlist_count {
        json["playlist_count"] = serde_json::json!(count);
    }
    json.to_string()
}

const H264: &str = "avc1.640028";
const VP9: &str = "vp09.00.40.08";
const AV1: &str = "av01.0.08M.08";
const AAC: &str = "mp4a.40.2";
const OPUS: &str = "opus";
/// What YouTube asks of its direct downloads.
const YOUTUBE_CHUNK_SIZE: u64 = 10_485_760;

fn video_format(base: &Url, id: &str, ext: &str, vcodec: &str, height: u32) -> FormatSpec {
    let kbps = f64::from(height) * 1.5;
    FormatSpec {
        format_id: id.to_string(),
        protocol: "https".to_string(),
        ext: ext.to_string(),
        vcodec: vcodec.to_string(),
        acodec: "none".to_string(),
        height: Some(height),
        tbr: Some(kbps),
        filesize: Some(u64::from(height) * 40_000),
        filesize_approx: None,
        language_preference: None,
        format_note: Some(format!("{height}p")),
        has_drm: false,
        http_chunk_size: Some(YOUTUBE_CHUNK_SIZE),
        url: media_url(base, &format!("media/{id}")),
    }
}

fn audio_format(base: &Url, id: &str, ext: &str, acodec: &str, preference: i32) -> FormatSpec {
    let dub = preference < 0;
    FormatSpec {
        format_id: id.to_string(),
        protocol: "https".to_string(),
        ext: ext.to_string(),
        vcodec: "none".to_string(),
        acodec: acodec.to_string(),
        height: None,
        tbr: Some(130.0),
        filesize: Some(3_000_000),
        filesize_approx: None,
        language_preference: Some(preference),
        format_note: Some(if dub { "dub, medium" } else { "medium" }.to_string()),
        has_drm: false,
        http_chunk_size: Some(YOUTUBE_CHUNK_SIZE),
        url: media_url(base, &format!("media/{id}")),
    }
}

/// The same video as a segmented stream, as yt-dlp lists it beside the direct one.
fn hls_twin(base: &Url, format: &FormatSpec) -> FormatSpec {
    let id = format!("hls-{}", format.format_id);
    FormatSpec {
        url: media_url(base, &format!("hls/{id}.m3u8")),
        format_id: id,
        protocol: "m3u8_native".to_string(),
        ext: "mp4".to_string(),
        http_chunk_size: None,
        ..format.clone()
    }
}

fn media_url(base: &Url, path: &str) -> String {
    base.join(path)
        .expect("a path joins a base URL")
        .to_string()
}

/// A page shaped like a YouTube video: H.264, VP9 and AV1 video-only formats
/// (each also as an `m3u8_native` stream), and original and dubbed audio.
/// Every format but dub `140-3` has an exact size; that one only an estimate.
pub fn youtube_like(base: &Url) -> PageSpec {
    let direct_video = [
        ("134", "mp4", H264, 360),
        ("135", "mp4", H264, 480),
        ("136", "mp4", H264, 720),
        ("137", "mp4", H264, 1080),
        ("247", "webm", VP9, 720),
        ("248", "webm", VP9, 1080),
        ("271", "webm", VP9, 1440),
        ("313", "webm", VP9, 2160),
        ("399", "mp4", AV1, 1080),
        ("701", "mp4", AV1, 2160),
    ]
    .map(|(id, ext, codec, height)| video_format(base, id, ext, codec, height));
    let mut dub = audio_format(base, "140-3", "m4a", AAC, -1);
    dub.filesize = None;
    dub.filesize_approx = Some(3_100_000);
    let audio = [
        audio_format(base, "140", "m4a", AAC, 10),
        audio_format(base, "140-0", "m4a", AAC, -1),
        dub,
        audio_format(base, "251", "webm", OPUS, 10),
        audio_format(base, "251-3", "webm", OPUS, -1),
    ];
    let hls: Vec<FormatSpec> = direct_video.iter().map(|f| hls_twin(base, f)).collect();
    PageSpec {
        title: "A test video".to_string(),
        extractor_key: "Youtube".to_string(),
        duration: 212,
        is_live: false,
        playlist_count: None,
        formats: [Vec::from(direct_video), hls, Vec::from(audio)].concat(),
    }
}
