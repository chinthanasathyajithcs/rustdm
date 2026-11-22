//! Test helpers for the video features: a stand-in for yt-dlp (and deno) that
//! plays scripted answers, a pretend yt-dlp `-J` page, and a world that has
//! installed the stand-in through the real tools manager.
//!
//! The stand-in is the `rdm-test-tool` program. It finds its script, its call
//! counter and its args log next to its own exe, never in the environment:
//! tests in one process run in parallel, and each has its own installed copy.

use super::fake_releases::{
    fake_version_in, publish_ytdlp, test_sources, zip_bytes_stored, zip_pin, FAKE_VERSION_MARKER,
};
use super::{Route, TestServer};
use crate::engine::build_client;
use crate::tools::{FetchCtx, Freshness, Installed, Tool, ToolStore};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use url::Url;

const SCRIPT_FILE: &str = "test-tool.json";
const COUNT_FILE: &str = "test-tool.count";
const ARGS_FILE: &str = "test-tool.args.jsonl";

/// The yt-dlp version `VideoWorld` installs.
pub const WORLD_YTDLP_VERSION: &str = "2026.11.10";
/// The deno version `VideoWorld` installs.
pub const WORLD_DENO_VERSION: &str = "2.9.7";

/// What `rdm-test-tool` does on each call. Call N plays `responses[min(N, len-1)]`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolScript {
    pub responses: Vec<ToolResponse>,
}

/// One scripted answer: wait, write the output, exit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub sleep_ms: u64,
    /// Print `stdout` this many times (when more than 1), for the oversize test.
    pub stdout_repeat: u64,
}

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
    let Ok(text) = std::fs::read_to_string(exe_dir(exe).join(ARGS_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| serde_json::from_str(line).expect("an args line is a JSON list"))
        .collect()
}

fn exe_dir(exe: &Path) -> &Path {
    exe.parent().expect("an exe has a folder")
}

/// The bytes of the stand-in program with `version` appended, so it can be
/// installed as yt-dlp or deno and answer `--version` with that version.
pub fn test_tool_program(version: &str) -> Vec<u8> {
    let path = std::env::var_os("CARGO_BIN_EXE_rdm-test-tool")
        .expect("CARGO_BIN_EXE_rdm-test-tool is set when integration tests run");
    let mut bytes = std::fs::read(path).expect("read rdm-test-tool");
    bytes.extend_from_slice(FAKE_VERSION_MARKER);
    bytes.extend_from_slice(version.as_bytes());
    bytes.push(0);
    bytes
}

/// The whole of `rdm-test-tool`: answer one call and return the exit code.
pub fn run_test_tool() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().expect("the test tool knows where it is");
    if args == ["--version"] {
        let version = tail_of(&exe).and_then(|bytes| fake_version_in(&bytes));
        return match version {
            Some(version) => {
                let _ = writeln!(std::io::stdout(), "{version}");
                0
            }
            None => 1,
        };
    }
    let dir = exe_dir(&exe);
    record_call(dir, &args);
    let response = next_response(dir);
    std::thread::sleep(std::time::Duration::from_millis(response.sleep_ms));
    // The reader may stop reading early (an oversize test); that is not an error here.
    let mut stdout = std::io::stdout().lock();
    for _ in 0..response.stdout_repeat.max(1) {
        let _ = stdout.write_all(response.stdout.as_bytes());
    }
    let _ = stdout.flush();
    let _ = std::io::stderr().write_all(response.stderr.as_bytes());
    response.exit_code
}

/// The last bytes of the file at `path`, where `test_tool_program` puts the
/// version. Scanning the whole program for it would take seconds in a debug build.
fn tail_of(path: &Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 4096;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn record_call(dir: &Path, args: &[String]) {
    let mut line = serde_json::to_string(args).expect("args serialize");
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(ARGS_FILE))
        .expect("open the args log");
    file.write_all(line.as_bytes()).expect("write the args log");
}

/// The answer for this call, and count the call.
fn next_response(dir: &Path) -> ToolResponse {
    let count_path = dir.join(COUNT_FILE);
    let call: usize = std::fs::read_to_string(&count_path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    std::fs::write(&count_path, (call + 1).to_string()).expect("write the call count");
    let script: ToolScript = std::fs::read(dir.join(SCRIPT_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    match script.responses.len() {
        0 => ToolResponse::default(),
        len => script.responses[call.min(len - 1)].clone(),
    }
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
