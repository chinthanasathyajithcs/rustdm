//! Putting one tool in place: download it with the engine into a staging
//! folder, check its SHA-256, unpack it, make sure it starts, then rename the
//! finished folder to `<root>/<tool>/<version>/`. Nothing in an existing
//! version folder is ever overwritten, so a running tool never blocks this.

use super::checksum::sha256_file;
use super::extract::{self, Rules};
use super::pins::{version_flag, Archive, Pin};
use super::release::YtDlpRelease;
use super::store_file::ToolsFile;
use super::{FetchCtx, Tool, ToolError};
use crate::engine::{self, DownloadRequest, Outcome, PauseReason};
use crate::error::Error;
use crate::ipc::protocol::ToolPhase;
use crate::state;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many connections a tool download opens.
const DOWNLOAD_CONNECTIONS: u32 = 4;
/// Times a download is resumed after GitHub's signed link expired.
const MAX_LINK_RESUMES: u32 = 3;
/// How long a tool may take to print its version.
const RUN_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Tries at starting a file that was only just written (Linux says "text file busy").
const RUN_CHECK_SPAWN_TRIES: u32 = 3;
const RUN_CHECK_SPAWN_RETRY_WAIT: Duration = Duration::from_millis(100);
/// Windows Defender may hold a new folder for a moment while it scans.
const RENAME_TRIES: u32 = 5;
const RENAME_RETRY_WAIT: Duration = Duration::from_millis(200);
/// `CREATE_NO_WINDOW`: do not flash a console window when running a tool.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const STAGING_DIR: &str = ".staging";

/// Everything needed to install one version of one tool.
#[derive(Clone, Debug)]
pub struct Plan {
    pub tool: Tool,
    pub version: String,
    pub url: url::Url,
    pub asset: String,
    pub sha256: String,
    pub archive: Archive,
    pub strip_components: usize,
    pub wanted: Vec<String>,
    /// The program, relative to the install folder.
    pub exe: String,
    /// The folder holding the program, relative to the install folder.
    pub exe_dir: String,
    pub unpacked_bytes: u64,
}

/// What a finished install leaves behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub version: String,
    /// SHA-256 of the program file.
    pub sha256: String,
}

pub fn ytdlp_exe_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

impl Plan {
    pub fn for_ytdlp(release: &YtDlpRelease) -> Plan {
        Plan {
            tool: Tool::YtDlp,
            version: release.version.clone(),
            url: release.url.clone(),
            asset: release.asset.clone(),
            sha256: release.sha256.clone(),
            archive: Archive::None,
            strip_components: 0,
            wanted: Vec::new(),
            exe: ytdlp_exe_name().to_string(),
            exe_dir: String::new(),
            unpacked_bytes: 0,
        }
    }

    pub fn for_pin(tool: Tool, pin: &Pin, download_base: &str) -> Result<Plan, ToolError> {
        let url = format!("{}/{}", download_base.trim_end_matches('/'), pin.path);
        let url = url::Url::parse(&url)
            .map_err(|e| ToolError::Unusable(format!("bad download address for {tool}: {e}")))?;
        Ok(Plan {
            tool,
            version: pin.version.clone(),
            url,
            asset: pin.asset.clone(),
            sha256: pin.sha256.clone(),
            archive: pin.archive,
            strip_components: pin.strip_components,
            wanted: pin.wanted.clone(),
            exe: pin.exe.clone(),
            exe_dir: pin.exe_dir.clone(),
            unpacked_bytes: pin.unpacked_bytes,
        })
    }

    /// The name of this install's folder in `.staging`, which is also how the
    /// store recognizes an install in progress.
    pub fn staging_name(&self) -> String {
        format!("{}-{}", self.tool, self.version)
    }
}

/// Where each version of `tool` lives.
pub fn version_dir(root: &Path, tool: Tool, version: &str) -> PathBuf {
    root.join(tool.name()).join(version)
}

/// Turn an engine failure into the tool error the user sees.
fn engine_error(tool: Tool, err: Error) -> ToolError {
    match err {
        Error::Cancelled => ToolError::Cancelled,
        Error::Http(e) => ToolError::network(tool, e),
        Error::Protocol(detail) | Error::State(detail) => ToolError::network(tool, detail),
        Error::FileChanged => ToolError::network(tool, "the file changed while downloading"),
        Error::InsufficientDiskSpace {
            target,
            required,
            available,
        } => ToolError::DiskFull {
            tool,
            target,
            needed: required,
            available,
        },
        Error::Io(source) => ToolError::Io { tool, source },
        Error::LinkExpired { status } => ToolError::Unusable(format!(
            "GitHub refused the {tool} download (HTTP {status})"
        )),
        other => ToolError::Unusable(other.to_string()),
    }
}

fn remove_download(target: &Path) {
    let _ = std::fs::remove_file(target);
    let _ = std::fs::remove_file(state::state_path(target));
    let _ = std::fs::remove_file(state::part_path(target));
}

async fn hash_file(tool: Tool, path: PathBuf) -> Result<String, ToolError> {
    tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|e| ToolError::Unusable(format!("hashing {tool} failed: {e}")))?
        .map_err(|source| ToolError::Io { tool, source })
}

/// Download the plan's file into `staging`, resuming what an earlier try left.
/// An already finished download with the right hash is reused as it is.
async fn fetch(
    client: &reqwest::Client,
    plan: &Plan,
    ctx: &FetchCtx,
    staging: &Path,
) -> Result<PathBuf, ToolError> {
    let tool = plan.tool;
    let target = staging.join(&plan.asset);
    let state_path = state::state_path(&target);

    if target.exists() {
        if hash_file(tool, target.clone()).await? == plan.sha256 {
            return Ok(target);
        }
        remove_download(&target);
    }

    let mut resumes = 0;
    loop {
        let outcome = if state_path.exists() {
            engine::resume_with_options(
                client,
                &state_path,
                None,
                Some(ctx.progress.clone()),
                ctx.cancel.clone(),
            )
            .await
        } else {
            // A part file with no state file cannot be continued.
            let _ = std::fs::remove_file(state::part_path(&target));
            engine::download(
                client,
                DownloadRequest {
                    url: plan.url.clone(),
                    template: "{downloads}/{name}".to_string(),
                    file_name: Some(plan.asset.clone()),
                    downloads_dir: staging.to_path_buf(),
                    connections: DOWNLOAD_CONNECTIONS,
                    progress: ctx.progress.clone(),
                    cancel: ctx.cancel.clone(),
                },
            )
            .await
        };
        match outcome.map_err(|e| engine_error(tool, e))? {
            Outcome::Completed { path, .. } => return Ok(path),
            Outcome::Paused {
                reason: PauseReason::Cancelled,
                ..
            } => return Err(ToolError::Cancelled),
            // The signed link behind github.com expired; asking again gets a new one.
            Outcome::Paused {
                reason: PauseReason::LinkExpired { status },
                ..
            } => {
                resumes += 1;
                if resumes > MAX_LINK_RESUMES {
                    return Err(ToolError::network(
                        tool,
                        format!("the download link kept expiring (HTTP {status})"),
                    ));
                }
            }
        }
    }
}

/// Start `exe` with the tool's version flag. It must exit 0 within 30 seconds.
async fn run_check(
    tool: Tool,
    exe: &Path,
    version: &str,
    kept: Option<&str>,
) -> Result<(), ToolError> {
    let failed = |code: Option<i32>| ToolError::RunCheckFailed {
        tool,
        version: version.to_string(),
        code,
        kept: kept.map(str::to_string),
    };
    let quarantined = || ToolError::Quarantined {
        file: exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| tool.name().to_string()),
    };

    let mut tries = 0;
    let mut child = loop {
        tries += 1;
        let mut command = tokio::process::Command::new(exe);
        command
            .arg(version_flag(tool))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        match command.spawn() {
            Ok(child) => break child,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && tries < RUN_CHECK_SPAWN_TRIES =>
            {
                tokio::time::sleep(RUN_CHECK_SPAWN_RETRY_WAIT).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(quarantined()),
            Err(_) => return Err(failed(None)),
        }
    };
    match tokio::time::timeout(RUN_CHECK_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(failed(status.code())),
        Ok(Err(_)) | Err(_) => Err(failed(None)),
    }
}

async fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        tries += 1;
        match tokio::fs::rename(from, to).await {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < RENAME_TRIES => {
                tokio::time::sleep(RENAME_RETRY_WAIT).await;
            }
            other => return other,
        }
    }
}

/// Lay out the downloaded file as an install folder at `out`.
async fn unpack(plan: &Plan, downloaded: &Path, out: &Path) -> Result<(), ToolError> {
    let tool = plan.tool;
    let io = |source| ToolError::Io { tool, source };
    let rules_wanted = plan.wanted.clone();
    match plan.archive {
        Archive::None => {
            std::fs::create_dir_all(out).map_err(io)?;
            let exe = out.join(&plan.exe);
            std::fs::rename(downloaded, &exe).map_err(io)?;
            extract::make_executable(&exe).map_err(io)
        }
        Archive::Zip => {
            let (downloaded, out) = (downloaded.to_path_buf(), out.to_path_buf());
            let strip_components = plan.strip_components;
            tokio::task::spawn_blocking(move || {
                extract::unpack_zip(
                    tool,
                    &downloaded,
                    &out,
                    &Rules {
                        strip_components,
                        wanted: &rules_wanted,
                    },
                )
            })
            .await
            .map_err(|e| ToolError::Unusable(format!("unpacking {tool} failed: {e}")))?
        }
        Archive::TarXz => {
            extract::unpack_tar_xz(
                tool,
                downloaded,
                out,
                &Rules {
                    strip_components: plan.strip_components,
                    wanted: &rules_wanted,
                },
                plan.unpacked_bytes,
            )
            .await
        }
    }
}

/// Install `plan` under `root`. `kept` is the version still in use, for the
/// message when the new one does not start. `on_phase` is told where we are.
///
/// On success the new version's folder is in place and nothing else is touched;
/// recording it in `tools.json` is the caller's job. On failure nothing is left
/// under `root/<tool>/`; a download that can still be resumed (cancelled or cut
/// off) stays in staging, and any other leftovers are removed.
pub async fn install(
    root: &Path,
    client: &reqwest::Client,
    plan: &Plan,
    ctx: &FetchCtx,
    kept: Option<&str>,
    on_phase: &(dyn Fn(ToolPhase) + Send + Sync),
) -> Result<Finished, ToolError> {
    let tool = plan.tool;
    let staging = root.join(STAGING_DIR).join(plan.staging_name());
    let out = staging.join("out");
    std::fs::create_dir_all(&staging).map_err(|source| ToolError::Io { tool, source })?;

    let result = install_in(root, client, plan, ctx, kept, on_phase, &staging, &out).await;
    match &result {
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&staging);
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&out);
            if !matches!(err, ToolError::Cancelled | ToolError::Network { .. }) {
                let _ = std::fs::remove_dir_all(&staging);
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn install_in(
    root: &Path,
    client: &reqwest::Client,
    plan: &Plan,
    ctx: &FetchCtx,
    kept: Option<&str>,
    on_phase: &(dyn Fn(ToolPhase) + Send + Sync),
    staging: &Path,
    out: &Path,
) -> Result<Finished, ToolError> {
    let tool = plan.tool;
    on_phase(ToolPhase::Downloading);
    let downloaded = fetch(client, plan, ctx, staging).await?;

    on_phase(ToolPhase::Verifying);
    if hash_file(tool, downloaded.clone()).await? != plan.sha256 {
        remove_download(&downloaded);
        return Err(ToolError::ChecksumMismatch {
            tool,
            version: plan.version.clone(),
        });
    }

    on_phase(ToolPhase::Installing);
    let _ = std::fs::remove_dir_all(out);
    unpack(plan, &downloaded, out).await?;
    let exe = out.join(&plan.exe);
    if !exe.is_file() {
        return Err(ToolError::Unusable(format!(
            "the {tool} download doesn't contain {}",
            plan.exe
        )));
    }
    run_check(tool, &exe, &plan.version, kept).await?;
    let sha256 = match plan.archive {
        Archive::None => plan.sha256.clone(),
        _ => hash_file(tool, exe.clone()).await?,
    };

    let final_dir = version_dir(root, tool, &plan.version);
    let io = |source| ToolError::Io { tool, source };
    if final_dir.exists() {
        // A damaged copy of this same version, being replaced.
        std::fs::remove_dir_all(&final_dir).map_err(io)?;
    }
    std::fs::create_dir_all(root.join(tool.name())).map_err(io)?;
    rename_with_retry(out, &final_dir).await.map_err(io)?;
    if !final_dir.join(&plan.exe).is_file() {
        let _ = std::fs::remove_dir_all(&final_dir);
        return Err(ToolError::Quarantined {
            file: Path::new(&plan.exe)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| plan.exe.clone()),
        });
    }
    Ok(Finished {
        version: plan.version.clone(),
        sha256,
    })
}

/// Best effort tidy-up, after each switch and at startup: remove version
/// folders that are neither `current` nor `previous`, and staging folders that
/// are not in `active` (names like `yt-dlp-2026.11.14`). A folder that cannot be
/// removed (on Windows, a running program holds it) is left for next time.
pub fn cleanup(root: &Path, file: &ToolsFile, active: &HashSet<String>) {
    for tool in Tool::ALL {
        let entry = file.entry(tool);
        let keep = |name: &str| {
            entry.current.as_deref() == Some(name)
                || entry.previous.as_deref() == Some(name)
                || active.contains(&format!("{tool}-{name}"))
        };
        remove_unlisted(&root.join(tool.name()), keep);
    }
    remove_unlisted(&root.join(STAGING_DIR), |name| active.contains(name));
}

fn remove_unlisted(dir: &Path, keep: impl Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep(&name) {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            tracing::debug!("could not remove {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::store_file::ToolEntry;

    fn make_dirs(root: &Path, names: &[&str]) {
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
    }

    #[test]
    fn cleanup_keeps_current_previous_and_installs_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_dirs(
            root,
            &[
                "yt-dlp/2026.11.14",
                "yt-dlp/2026.11.10",
                "yt-dlp/2026.11.01",
                "yt-dlp/2026.11.15",
                "ffmpeg/8.1.2",
                ".staging/yt-dlp-2026.11.15",
                ".staging/deno-2.9.8",
            ],
        );
        let file = ToolsFile::default().with_entry(
            Tool::YtDlp,
            ToolEntry {
                current: Some("2026.11.14".into()),
                previous: Some("2026.11.10".into()),
                ..ToolEntry::default()
            },
        );
        let active: HashSet<String> = ["yt-dlp-2026.11.15".to_string()].into();

        cleanup(root, &file, &active);

        assert!(root.join("yt-dlp/2026.11.14").exists());
        assert!(root.join("yt-dlp/2026.11.10").exists());
        assert!(root.join("yt-dlp/2026.11.15").exists(), "in progress");
        assert!(!root.join("yt-dlp/2026.11.01").exists());
        assert!(
            !root.join("ffmpeg/8.1.2").exists(),
            "not listed in tools.json"
        );
        assert!(root.join(".staging/yt-dlp-2026.11.15").exists());
        assert!(!root.join(".staging/deno-2.9.8").exists());
    }
}
