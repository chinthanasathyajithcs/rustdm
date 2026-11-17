//! Putting one tool in place: download it with the engine into a staging
//! folder, check its SHA-256, unpack it, make sure it starts, then rename the
//! finished folder to `<root>/<tool>/<version>/`. A folder that already holds
//! that version is moved aside first and only deleted once the new one is in
//! place, so a failed install never loses a version `tools.json` lists.

use super::extract::{self, Rules};
use super::fetch::{fetch, hash_file, remove_download};
use super::pins::{version_flag, Archive, Pin};
use super::release::YtDlpRelease;
use super::store_file::ToolsFile;
use super::{version, FetchCtx, Tool, ToolError};
use crate::ipc::protocol::ToolPhase;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a tool may take to print its version.
const RUN_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Tries at starting a file that was only just written (Linux says "text file busy").
const RUN_CHECK_SPAWN_TRIES: u32 = 3;
const RUN_CHECK_SPAWN_RETRY_WAIT: Duration = Duration::from_millis(100);
/// Windows Defender may hold a new folder for a moment while it scans.
const RENAME_TRIES: u32 = 5;
const RENAME_RETRY_WAIT: Duration = Duration::from_millis(200);
/// The most of a tool's version output that is read.
const VERSION_OUTPUT_CAP: usize = 256;
/// `CREATE_NO_WINDOW`: do not flash a console window when running a tool.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const STAGING_DIR: &str = ".staging";
/// Staging folders named this plus an install's name hold a folder being replaced.
const REPLACED_PREFIX: &str = "replaced-";

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
    /// What the program must print for its version flag (trimmed), or the
    /// install is refused. Set for yt-dlp, whose version comes from an unsigned
    /// release tag and so is checked against the program itself.
    pub expected_output: Option<String>,
    /// When yt-dlp's key signed this release's checksum list (unix seconds).
    pub signed_at: Option<u64>,
}

/// What a finished install leaves behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub version: String,
    /// SHA-256 of the program file.
    pub sha256: String,
}

/// What an install needs to know, lent for the length of the call.
pub struct InstallCtx<'a> {
    pub root: &'a Path,
    pub client: &'a reqwest::Client,
    pub plan: &'a Plan,
    pub fetch: &'a FetchCtx,
    /// The version still in use, for the message when the new one does not start.
    pub kept: Option<&'a str>,
    /// Told where the install is.
    pub on_phase: &'a (dyn Fn(ToolPhase) + Send + Sync),
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
            expected_output: Some(release.version.clone()),
            signed_at: Some(release.signed_at),
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
            expected_output: None,
            signed_at: None,
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

/// Start `exe` with the tool's version flag. It must exit 0 within 30 seconds,
/// and when the plan says what it must print, print exactly that.
async fn run_check(ctx: &InstallCtx<'_>, exe: &Path) -> Result<(), ToolError> {
    let (plan, tool) = (ctx.plan, ctx.plan.tool);
    let failed = |code: Option<i32>| ToolError::RunCheckFailed {
        tool,
        version: plan.version.clone(),
        code,
        kept: ctx.kept.map(str::to_string),
    };
    let wants_output = plan.expected_output.is_some();

    let mut tries = 0;
    let child = loop {
        tries += 1;
        let mut command = tokio::process::Command::new(exe);
        command
            .arg(version_flag(tool))
            .stdin(std::process::Stdio::null())
            .stdout(if wants_output {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::quarantined(exe))
            }
            Err(_) => return Err(failed(None)),
        }
    };
    match tokio::time::timeout(RUN_CHECK_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => match &plan.expected_output {
            Some(expected) => check_reported_version(tool, expected, &output.stdout),
            None => Ok(()),
        },
        Ok(Ok(output)) => Err(failed(output.status.code())),
        Ok(Err(_)) | Err(_) => Err(failed(None)),
    }
}

/// The program must say it is `expected`, and nothing else.
fn check_reported_version(tool: Tool, expected: &str, stdout: &[u8]) -> Result<(), ToolError> {
    let shown = &stdout[..stdout.len().min(VERSION_OUTPUT_CAP)];
    let reported = String::from_utf8_lossy(shown).trim().to_string();
    if reported == expected && stdout.len() <= VERSION_OUTPUT_CAP {
        return Ok(());
    }
    Err(ToolError::VersionMismatch {
        tool,
        expected: expected.to_string(),
        reported,
    })
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
            tokio::fs::create_dir_all(out).await.map_err(io)?;
            let exe = out.join(&plan.exe);
            tokio::fs::rename(downloaded, &exe).await.map_err(io)?;
            let for_mode = exe.clone();
            tokio::task::spawn_blocking(move || extract::make_executable(&for_mode))
                .await
                .map_err(|e| ToolError::Unusable(format!("unpacking {tool} failed: {e}")))?
                .map_err(io)
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

async fn remove_dir_quietly(dir: &Path) {
    let _ = tokio::fs::remove_dir_all(dir).await;
}

/// Install `ctx.plan` under `ctx.root`.
///
/// On success the new version's folder is in place and nothing else is touched;
/// recording it in `tools.json` is the caller's job. On failure nothing is left
/// under `root/<tool>/` that was not there before; a download that can still be
/// resumed (cancelled or cut off) stays in staging, and any other leftovers are
/// removed.
pub async fn install(ctx: &InstallCtx<'_>) -> Result<Finished, ToolError> {
    let tool = ctx.plan.tool;
    let staging = ctx.root.join(STAGING_DIR).join(ctx.plan.staging_name());
    let out = staging.join("out");
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|source| ToolError::Io { tool, source })?;

    let result = install_in(ctx, &staging, &out).await;
    match &result {
        Ok(_) => remove_dir_quietly(&staging).await,
        Err(err) => {
            remove_dir_quietly(&out).await;
            if !matches!(err, ToolError::Cancelled | ToolError::Network { .. }) {
                remove_dir_quietly(&staging).await;
            }
        }
    }
    result
}

async fn install_in(
    ctx: &InstallCtx<'_>,
    staging: &Path,
    out: &Path,
) -> Result<Finished, ToolError> {
    let (plan, tool) = (ctx.plan, ctx.plan.tool);
    (ctx.on_phase)(ToolPhase::Downloading);
    let downloaded = fetch(ctx, staging).await?;

    (ctx.on_phase)(ToolPhase::Verifying);
    if hash_file(tool, downloaded.clone()).await? != plan.sha256 {
        remove_download(&downloaded).await;
        return Err(ToolError::ChecksumMismatch {
            tool,
            version: plan.version.clone(),
        });
    }

    (ctx.on_phase)(ToolPhase::Installing);
    remove_dir_quietly(out).await;
    unpack(plan, &downloaded, out).await?;
    let exe = out.join(&plan.exe);
    if !tokio::fs::metadata(&exe).await.is_ok_and(|m| m.is_file()) {
        return Err(ToolError::Unusable(format!(
            "the {tool} download doesn't contain {}",
            plan.exe
        )));
    }
    run_check(ctx, &exe).await?;
    let sha256 = match plan.archive {
        Archive::None => plan.sha256.clone(),
        _ => hash_file(tool, exe.clone()).await?,
    };
    put_in_place(ctx, out).await?;
    Ok(Finished {
        version: plan.version.clone(),
        sha256,
    })
}

/// Rename `out` to the version's folder. An existing folder of that version (a
/// damaged copy being replaced, or one `tools.json` still lists) is moved aside
/// first and deleted only once the new one is in place; if that fails it is put
/// back.
async fn put_in_place(ctx: &InstallCtx<'_>, out: &Path) -> Result<(), ToolError> {
    let (plan, tool) = (ctx.plan, ctx.plan.tool);
    let io = |source| ToolError::Io { tool, source };
    let final_dir = version_dir(ctx.root, tool, &plan.version);
    let aside = ctx
        .root
        .join(STAGING_DIR)
        .join(format!("{REPLACED_PREFIX}{}", plan.staging_name()));

    let had_old = tokio::fs::try_exists(&final_dir).await.unwrap_or(false);
    if had_old {
        remove_dir_quietly(&aside).await;
        rename_with_retry(&final_dir, &aside).await.map_err(io)?;
    }
    let placed = place(ctx, out, &final_dir).await;
    match (&placed, had_old) {
        (Ok(()), true) => remove_dir_quietly(&aside).await,
        (Err(_), true) => {
            remove_dir_quietly(&final_dir).await;
            let _ = rename_with_retry(&aside, &final_dir).await;
        }
        _ => {}
    }
    placed
}

async fn place(ctx: &InstallCtx<'_>, out: &Path, final_dir: &Path) -> Result<(), ToolError> {
    let (plan, tool) = (ctx.plan, ctx.plan.tool);
    let io = |source| ToolError::Io { tool, source };
    tokio::fs::create_dir_all(ctx.root.join(tool.name()))
        .await
        .map_err(io)?;
    rename_with_retry(out, final_dir).await.map_err(io)?;
    let program = final_dir.join(&plan.exe);
    if !tokio::fs::metadata(&program)
        .await
        .is_ok_and(|m| m.is_file())
    {
        remove_dir_quietly(final_dir).await;
        return Err(ToolError::quarantined(Path::new(&plan.exe)));
    }
    Ok(())
}

/// What `cleanup` must not touch beyond what `tools.json` lists.
pub struct Keep<'a> {
    /// Whether a staging name is an install in progress. Asked again for each
    /// folder just before it would be removed, so an install that starts while
    /// the clean-up runs is not caught.
    pub is_active: &'a dyn Fn(&str) -> bool,
    /// The build each pinned tool would install, whose half-finished download
    /// is kept so a cancelled one can be resumed.
    pub pinned: &'a HashMap<Tool, String>,
}

/// Best effort tidy-up, after each switch and at startup: remove version
/// folders that are neither `current` nor `previous`, and staging folders that
/// are not in progress. Staging is kept for what could still be resumed: the
/// pinned build of ffmpeg or deno, and the newest yt-dlp newer than the one
/// installed. A folder that cannot be removed (on Windows, a running program
/// holds it) is left for next time.
pub fn cleanup(root: &Path, file: &ToolsFile, keep: &Keep<'_>) {
    for tool in Tool::ALL {
        let entry = file.entry(tool);
        let listed = |name: &str| {
            entry.current.as_deref() == Some(name)
                || entry.previous.as_deref() == Some(name)
                || (keep.is_active)(&format!("{tool}-{name}"))
        };
        remove_unlisted(&root.join(tool.name()), listed);
    }
    let staging = root.join(STAGING_DIR);
    let ytdlp_current = file.entry(Tool::YtDlp).current;
    let resumable_ytdlp = newest_ytdlp_staging(&staging, ytdlp_current.as_deref());
    remove_unlisted(&staging, |name| {
        let name = name.strip_prefix(REPLACED_PREFIX).unwrap_or(name);
        (keep.is_active)(name)
            || resumable_ytdlp.as_deref() == Some(name)
            || keep
                .pinned
                .iter()
                .any(|(tool, version)| name == format!("{tool}-{version}"))
    });
}

/// The staging folder of the newest yt-dlp that is newer than `current`.
fn newest_ytdlp_staging(staging: &Path, current: Option<&str>) -> Option<String> {
    let prefix = format!("{}-", Tool::YtDlp);
    let mut best: Option<(Vec<u64>, String)> = None;
    for entry in std::fs::read_dir(staging).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(found) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(parsed) = version::parse(found) else {
            continue;
        };
        if current.is_some_and(|current| !version::is_newer(found, current)) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(top, _)| version::compare(&parsed, top).is_gt())
        {
            best = Some((parsed, name));
        }
    }
    best.map(|(_, name)| name)
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
    use std::collections::HashSet;

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

        cleanup(
            root,
            &file,
            &Keep {
                is_active: &|name| active.contains(name),
                pinned: &HashMap::new(),
            },
        );

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

    #[test]
    fn cleanup_keeps_what_a_cancelled_download_could_resume() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_dirs(
            root,
            &[
                ".staging/yt-dlp-2026.11.14",
                ".staging/yt-dlp-2026.11.15",
                ".staging/yt-dlp-2026.11.16",
                ".staging/yt-dlp-2026.11.10",
                ".staging/deno-2.9.8",
                ".staging/deno-2.9.7",
                ".staging/ffmpeg-8.1.3",
                ".staging/replaced-yt-dlp-2026.11.20",
            ],
        );
        let file = ToolsFile::default().with_entry(
            Tool::YtDlp,
            ToolEntry {
                current: Some("2026.11.14".into()),
                ..ToolEntry::default()
            },
        );
        let pinned: HashMap<Tool, String> = [(Tool::Deno, "2.9.8".to_string())].into();

        cleanup(
            root,
            &file,
            &Keep {
                is_active: &|_| false,
                pinned: &pinned,
            },
        );

        let left = |name: &str| root.join(".staging").join(name).exists();
        // Only the newest yt-dlp that is newer than the installed one stays.
        assert!(left("yt-dlp-2026.11.16"));
        assert!(!left("yt-dlp-2026.11.15"));
        assert!(!left("yt-dlp-2026.11.14"), "already installed");
        assert!(!left("yt-dlp-2026.11.10"));
        // The pinned build stays; other versions and unpinned tools do not.
        assert!(left("deno-2.9.8"));
        assert!(!left("deno-2.9.7"));
        assert!(!left("ffmpeg-8.1.3"));
        assert!(!left("replaced-yt-dlp-2026.11.20"));
    }

    #[test]
    fn a_program_must_print_exactly_the_version_it_is_installed_as() {
        let ok = |out: &[u8]| check_reported_version(Tool::YtDlp, "2026.11.14.1", out);
        assert!(ok(b"2026.11.14.1\n").is_ok());
        assert!(ok(b"2026.11.14.1\r\n").is_ok());
        for bad in [
            &b"9999.12.31\n"[..],
            b"",
            b"2026.11.14.1 extra\n",
            b"2026.11.14\n",
            b"x\n2026.11.14.1\n",
        ] {
            assert!(
                matches!(ok(bad), Err(ToolError::VersionMismatch { .. })),
                "{bad:?}"
            );
        }
        let flood = vec![b'1'; 10_000];
        assert!(ok(&flood).is_err());
    }

    fn test_plan(version: &str) -> Plan {
        Plan {
            tool: Tool::Deno,
            version: version.to_string(),
            url: url::Url::parse("http://127.0.0.1:1/deno.zip").unwrap(),
            asset: "deno.zip".into(),
            sha256: String::new(),
            archive: Archive::Zip,
            strip_components: 0,
            wanted: vec!["deno".into()],
            exe: "deno".into(),
            exe_dir: String::new(),
            unpacked_bytes: 0,
            expected_output: None,
            signed_at: None,
        }
    }

    #[tokio::test]
    async fn a_listed_folder_is_replaced_only_when_the_new_one_is_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (client, fetch, plan) = (reqwest::Client::new(), FetchCtx::new(), test_plan("2.9.8"));
        let ctx = InstallCtx {
            root,
            client: &client,
            plan: &plan,
            fetch: &fetch,
            kept: None,
            on_phase: &|_| {},
        };
        let listed = root.join("deno/2.9.8");
        std::fs::create_dir_all(&listed).unwrap();
        std::fs::write(listed.join("deno"), b"old").unwrap();

        // The new folder is not there to be renamed: the old one comes back.
        let missing = root.join(".staging/deno-2.9.8/out");
        assert!(put_in_place(&ctx, &missing).await.is_err());
        assert_eq!(std::fs::read(listed.join("deno")).unwrap(), b"old");

        // With a good one, it takes over and the moved-aside copy is gone.
        let out = root.join(".staging/deno-2.9.8/out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("deno"), b"new").unwrap();
        put_in_place(&ctx, &out).await.unwrap();
        assert_eq!(std::fs::read(listed.join("deno")).unwrap(), b"new");
        assert!(!root.join(".staging/replaced-deno-2.9.8").exists());

        // A folder that comes without the program is refused and the old one stays.
        let empty = root.join(".staging/deno-2.9.8/out");
        std::fs::create_dir_all(&empty).unwrap();
        let refused = put_in_place(&ctx, &empty).await.unwrap_err();
        assert!(
            matches!(refused, ToolError::Quarantined { .. }),
            "{refused}"
        );
        assert_eq!(std::fs::read(listed.join("deno")).unwrap(), b"new");
    }
}
