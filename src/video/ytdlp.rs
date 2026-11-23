//! Runs yt-dlp to look a video page up. It starts an outside program and may
//! hand it the user's browser cookies, so the command line is built in one
//! place, the link always comes after `--`, and the cookies file is private to
//! the user and gone as soon as yt-dlp is.

use super::errors::LookupFailure;
use super::info::{parse_page, PageInfo};
use crate::cancel::or_cancel;
use crate::request_context::ContextCookie;
use crate::tools::Installed;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use url::Url;

/// How long a lookup may take before yt-dlp is stopped.
pub const LOOKUP_TIME_LIMIT: Duration = Duration::from_secs(60);
/// The most output read from yt-dlp; more is refused.
pub const MAX_LOOKUP_OUTPUT: usize = 32 * 1024 * 1024;
/// The last part of yt-dlp's error output that is kept.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// How long to wait for the error output after yt-dlp has exited.
const STDERR_GRACE: Duration = Duration::from_secs(2);
/// Tries at an unused cookies file name before giving up.
const COOKIES_NAME_TRIES: usize = 8;
const READ_CHUNK: usize = 64 * 1024;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What to look up and how to run yt-dlp for it.
pub struct LookupRequest<'a> {
    pub ytdlp: &'a Installed,
    pub deno: &'a Installed,
    pub page_url: &'a Url,
    /// The browser's cookies for the page; empty when the download has none.
    pub cookies: &'a [ContextCookie],
    pub user_agent: Option<&'a str>,
    pub referer: Option<&'a str>,
    /// A private folder for the cookies file.
    pub cookies_dir: &'a Path,
    pub cancel: Option<watch::Receiver<bool>>,
}

/// The command line, in the one order rustdm uses. `cookies_file` is the file
/// made for this run, if any. The link is last, after `--`, so a link that
/// starts with a dash is still a link.
pub fn lookup_args(req: &LookupRequest, cookies_file: Option<&Path>) -> Vec<OsString> {
    let mut deno = OsString::from("deno:");
    deno.push(&req.deno.exe);
    let mut args: Vec<OsString> = ["--ignore-config", "--no-js-runtimes", "--js-runtimes"]
        .map(OsString::from)
        .into();
    args.push(deno);
    // --flat-playlist: a playlist link is listed, not opened video by video
    // (that took over the time limit on a 38-video playlist).
    args.extend(["-J", "--no-playlist", "--flat-playlist", "--no-warnings"].map(OsString::from));
    if let Some(file) = cookies_file {
        args.push("--cookies".into());
        args.push(file.into());
    }
    if let Some(user_agent) = req.user_agent.filter(|v| is_plain(v)) {
        args.extend(["--user-agent".into(), user_agent.into()]);
    }
    if let Some(referer) = req.referer.filter(|v| is_plain(v)) {
        args.extend(["--referer".into(), referer.into()]);
    }
    args.push("--".into());
    args.push(req.page_url.as_str().into());
    args
}

/// Header values come from the browser's request; one with a control
/// character in it is left out rather than passed on.
fn is_plain(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

/// Run yt-dlp on the page and read what it found.
pub async fn look_up(req: LookupRequest<'_>) -> Result<PageInfo, LookupFailure> {
    look_up_within(req, LOOKUP_TIME_LIMIT).await
}

/// `look_up` with a shorter time limit, so a test need not wait a minute.
#[cfg(feature = "testing")]
pub async fn look_up_with_limit(
    req: LookupRequest<'_>,
    limit: Duration,
) -> Result<PageInfo, LookupFailure> {
    look_up_within(req, limit).await
}

async fn look_up_within(
    req: LookupRequest<'_>,
    limit: Duration,
) -> Result<PageInfo, LookupFailure> {
    // Declared first, so it is dropped (and the file removed) after the child.
    let cookies_file = write_cookies_file(&req)?;
    let args = lookup_args(&req, cookies_file.as_ref().map(|file| file.path.as_path()));
    tracing::debug!(
        cookies = req.cookies.len(),
        has_cookies_file = cookies_file.is_some(),
        "looking up a video page"
    );
    let output = run_ytdlp(&req.ytdlp.exe, &args, limit, req.cancel.as_ref()).await?;
    if !output.status.success() {
        return Err(LookupFailure::Exited {
            code: output.status.code(),
            stderr: output.stderr,
        });
    }
    parse_page(&output.stdout).map_err(LookupFailure::NotJson)
}

fn write_cookies_file(req: &LookupRequest) -> Result<Option<CookiesFile>, LookupFailure> {
    if req.cookies.is_empty() {
        return Ok(None);
    }
    let text = netscape_text(req.cookies);
    CookiesFile::create(req.cookies_dir, &text)
        .map(Some)
        .map_err(|e| LookupFailure::Spawn(format!("couldn't save your sign-in for yt-dlp: {e}")))
}

/// A cookies file that removes itself when dropped.
struct CookiesFile {
    path: PathBuf,
}

impl CookiesFile {
    /// Make a new file only the user can read, and fill it.
    fn create(dir: &Path, text: &str) -> std::io::Result<CookiesFile> {
        for _ in 0..COOKIES_NAME_TRIES {
            let path = dir.join(format!("lookup-{:016x}.txt", fastrand::u64(..)));
            let mut file = match open_private(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            // From here the guard owns the file, so a failed write removes it too.
            let guard = CookiesFile { path };
            file.write_all(text.as_bytes())?;
            file.flush()?;
            return Ok(guard);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no unused cookies file name",
        ))
    }
}

impl Drop for CookiesFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                "couldn't remove the cookies file: {e}"
            ),
        }
    }
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// The data folder's access list already limits it to the user.
#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The cookies in Netscape format, which `--cookies` reads. A cookie with a
/// tab or line break in it would break the format (or add a cookie of its
/// own), so it is left out.
fn netscape_text(cookies: &[ContextCookie]) -> String {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    let mut skipped = 0;
    for cookie in cookies {
        match netscape_line(cookie) {
            Some(line) => {
                text.push_str(&line);
                text.push('\n');
            }
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "left cookies with control characters out");
    }
    text
}

fn netscape_line(cookie: &ContextCookie) -> Option<String> {
    let fields = [&cookie.name, &cookie.value, &cookie.domain, &cookie.path];
    if fields.iter().any(|f| f.chars().any(char::is_control)) {
        return None;
    }
    let bare = cookie.domain.trim_start_matches('.');
    let (domain, subdomains) = if cookie.host_only {
        (bare.to_string(), "FALSE")
    } else {
        (format!(".{bare}"), "TRUE")
    };
    let secure = if cookie.secure { "TRUE" } else { "FALSE" };
    let expiry = cookie.expires.unwrap_or(0).max(0);
    Some(format!(
        "{domain}\t{subdomains}\t{}\t{secure}\t{expiry}\t{}\t{}",
        cookie.path, cookie.name, cookie.value
    ))
}

struct YtdlpOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

fn spawn_ytdlp(exe: &Path, args: &[OsString]) -> std::io::Result<Child> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.spawn()
}

/// Start yt-dlp and wait for it, within `limit` and unless `cancel` fires.
/// Whatever stops the wait, the child is killed and reaped before returning.
async fn run_ytdlp(
    exe: &Path,
    args: &[OsString],
    limit: Duration,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<YtdlpOutput, LookupFailure> {
    let mut child = spawn_ytdlp(exe, args).map_err(|e| LookupFailure::Spawn(e.to_string()))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(read_tail(stderr, MAX_STDERR_BYTES));

    let waited = or_cancel(
        tokio::time::timeout(limit, read_and_wait(&mut child, stdout)),
        cancel,
    )
    .await;
    let finished = match waited {
        Ok(Ok(finished)) => finished,
        Ok(Err(_elapsed)) => {
            return Err(stop(&mut child, stderr_task, LookupFailure::TimedOut).await)
        }
        Err(_cancelled) => {
            let cancelled = LookupFailure::Spawn("cancelled".to_string());
            return Err(stop(&mut child, stderr_task, cancelled).await);
        }
    };
    match finished {
        Ok((status, stdout)) => {
            let mut stderr_task = stderr_task;
            let stderr = match tokio::time::timeout(STDERR_GRACE, &mut stderr_task).await {
                Ok(read) => read.unwrap_or_default(),
                Err(_grace_over) => {
                    stderr_task.abort();
                    String::new()
                }
            };
            Ok(YtdlpOutput {
                status,
                stdout,
                stderr,
            })
        }
        Err(failure) => Err(stop(&mut child, stderr_task, failure).await),
    }
}

/// Kill the child, wait until it is gone, and give back `failure`.
async fn stop(
    child: &mut Child,
    stderr_task: tokio::task::JoinHandle<String>,
    failure: LookupFailure,
) -> LookupFailure {
    if let Err(e) = child.kill().await {
        tracing::warn!("couldn't stop yt-dlp: {e}");
    }
    stderr_task.abort();
    failure
}

/// Read all of stdout (up to the cap), then wait for the exit.
async fn read_and_wait(
    child: &mut Child,
    stdout: impl AsyncRead + Unpin,
) -> Result<(ExitStatus, Vec<u8>), LookupFailure> {
    let bytes = read_capped(stdout, MAX_LOOKUP_OUTPUT).await?;
    let status = child
        .wait()
        .await
        .map_err(|e| LookupFailure::Spawn(e.to_string()))?;
    Ok((status, bytes))
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    cap: usize,
) -> Result<Vec<u8>, LookupFailure> {
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| LookupFailure::Spawn(e.to_string()))?;
        if n == 0 {
            return Ok(bytes);
        }
        if bytes.len() + n > cap {
            return Err(LookupFailure::TooMuchOutput);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// Read to the end, keeping only the last `keep` bytes.
pub(super) async fn read_tail(mut reader: impl AsyncRead + Unpin, keep: usize) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > keep {
            tail.drain(..tail.len() - keep);
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}
