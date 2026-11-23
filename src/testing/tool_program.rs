//! The whole of `rdm-test-tool`, the stand-in for yt-dlp and deno in tests,
//! and the types its scripts and logs use.
//!
//! This file uses only std, serde and serde_json: `src/bin/rdm-test-tool.rs`
//! includes it with `#[path]` instead of linking the rustdm library, which
//! keeps the program small. Each video test world installs two copies of it.
//!
//! The stand-in finds its script, its call counter and its args log next to
//! its own exe, never in the environment: tests in one process run in
//! parallel, and each has its own installed copy.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

pub const SCRIPT_FILE: &str = "test-tool.json";
pub const COUNT_FILE: &str = "test-tool.count";
pub const ARGS_FILE: &str = "test-tool.args.jsonl";
/// Marks a script carried inside the program itself (see `test_tool_program_scripted`).
pub const SCRIPT_MARKER: &[u8] = b"\0rdm-test-script=";
/// Marks the version appended to a fake program; the version ends at a NUL byte.
pub const VERSION_MARKER: &[u8] = b"\0rdm-fake-version=";
/// The most of the program's end searched for an embedded script.
const SCRIPT_TAIL_BYTES: u64 = 1024 * 1024;
/// The most of the program's end searched for the version.
const VERSION_TAIL_BYTES: u64 = 4096;

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

/// What the stand-in saw in the file named by `--cookies`, when it was called.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookiesSeen {
    pub cookies_file: String,
    /// The file's Unix permission bits as the OS reports them; 0 on Windows.
    pub mode: u32,
    pub content: String,
}

/// The version after the last `VERSION_MARKER` in `program`, if it has one.
pub fn version_in(program: &[u8]) -> Option<String> {
    let at = program
        .windows(VERSION_MARKER.len())
        .rposition(|window| window == VERSION_MARKER)?;
    let rest = &program[at + VERSION_MARKER.len()..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    String::from_utf8(rest[..end].to_vec()).ok()
}

pub fn exe_dir(exe: &Path) -> &Path {
    exe.parent().expect("an exe has a folder")
}

/// Answer one call and return the exit code.
pub fn run_test_tool() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().expect("the test tool knows where it is");
    if args == ["--version"] {
        let version = tail_of(&exe, VERSION_TAIL_BYTES).and_then(|bytes| version_in(&bytes));
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

/// The last `bytes` of the file at `path`, where the version and an embedded
/// script are put. Scanning the whole program would be slow.
fn tail_of(path: &Path, bytes: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(bytes))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The script carried inside this program, if it was made with
/// `test_tool_program_scripted`.
fn embedded_script() -> Option<ToolScript> {
    let exe = std::env::current_exe().ok()?;
    let tail = tail_of(&exe, SCRIPT_TAIL_BYTES)?;
    let at = tail
        .windows(SCRIPT_MARKER.len())
        .rposition(|window| window == SCRIPT_MARKER)?;
    let rest = &tail[at + SCRIPT_MARKER.len()..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    serde_json::from_slice(&rest[..end]).ok()
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
    record_cookies_file(&mut file, args);
}

/// When the call names a `--cookies` file that exists, log what it holds now,
/// so a test can tell the file was there, private and filled while the tool ran.
fn record_cookies_file(log: &mut std::fs::File, args: &[String]) {
    let Some(path) = args
        .windows(2)
        .find(|pair| pair[0] == "--cookies")
        .map(|pair| pair[1].clone())
    else {
        return;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return;
    };
    let seen = CookiesSeen {
        cookies_file: path.clone(),
        mode: file_mode(&metadata),
        content: std::fs::read_to_string(&path).unwrap_or_default(),
    };
    let mut line = serde_json::to_string(&seen).expect("a cookies record serializes");
    line.push('\n');
    log.write_all(line.as_bytes()).expect("write the args log");
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
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
        .or_else(embedded_script)
        .unwrap_or_default();
    match script.responses.len() {
        0 => ToolResponse::default(),
        len => script.responses[call.min(len - 1)].clone(),
    }
}
