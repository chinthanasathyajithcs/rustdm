//! Getting a tool's file onto disk: the engine downloads it into a staging
//! folder, resuming what an earlier try left, and a finished file with the
//! right hash is reused as it is.

use super::checksum::sha256_file;
use super::install::{InstallCtx, Plan};
use super::{Tool, ToolError};
use crate::engine::{self, DownloadRequest, Outcome, PauseReason};
use crate::error::Error;
use crate::state;
use std::path::{Path, PathBuf};

/// How many connections a tool download opens.
const DOWNLOAD_CONNECTIONS: u32 = 4;
/// Times a download is resumed after GitHub's signed link expired.
const MAX_LINK_RESUMES: u32 = 3;

/// Turn an engine failure into the tool error the user sees.
pub(super) fn engine_error(tool: Tool, err: Error) -> ToolError {
    match err {
        Error::Cancelled => ToolError::Cancelled,
        Error::Http(e) => ToolError::network(tool, e),
        Error::Protocol(detail) => ToolError::network(tool, detail),
        Error::InsufficientDiskSpace {
            target,
            required,
            available,
        } => ToolError::disk_full(tool, &target, required, available),
        Error::Io(source) => ToolError::Io { tool, source },
        Error::LinkExpired { status } => ToolError::Unusable(format!(
            "GitHub refused the {tool} download (HTTP {status})"
        )),
        other => ToolError::Unusable(other.to_string()),
    }
}

/// Delete a download and everything the engine keeps beside it.
pub(super) async fn remove_download(target: &Path) {
    let _ = tokio::fs::remove_file(target).await;
    let _ = tokio::fs::remove_file(state::state_path(target)).await;
    let _ = tokio::fs::remove_file(state::part_path(target)).await;
}

pub(super) async fn hash_file(tool: Tool, path: PathBuf) -> Result<String, ToolError> {
    tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|e| ToolError::Unusable(format!("hashing {tool} failed: {e}")))?
        .map_err(|source| ToolError::Io { tool, source })
}

/// Whether a failed try says the saved download can never be continued, so the
/// only way on is to start over: its state file is unreadable, the file on the
/// server changed, or the state file is there without its data.
fn cannot_be_continued(err: &Error, resuming: bool) -> bool {
    match err {
        Error::State(_) | Error::FileChanged => true,
        Error::Io(e) => resuming && e.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// Allow one restart from zero. A second failure of the same kind is final, and
/// not worth retrying by itself, so it is not a network error.
fn allow_restart(tool: Tool, restarted: &mut bool, err: &Error) -> Result<(), ToolError> {
    if *restarted {
        return Err(ToolError::Unusable(format!(
            "the {tool} download could not be resumed or restarted ({err})"
        )));
    }
    *restarted = true;
    tracing::warn!("restarting the {tool} download from the start: {err}");
    Ok(())
}

/// One run of the engine: continue the saved download, or begin a new one.
async fn run_engine(
    ctx: &InstallCtx<'_>,
    staging: &Path,
    target: &Path,
    resuming: bool,
) -> Result<Outcome, Error> {
    let (plan, fetch) = (ctx.plan, ctx.fetch);
    if resuming {
        return engine::resume_with_options(
            ctx.client,
            &state::state_path(target),
            None,
            Some(fetch.progress.clone()),
            fetch.cancel.clone(),
        )
        .await;
    }
    // A part file with no state file cannot be continued.
    let _ = tokio::fs::remove_file(state::part_path(target)).await;
    engine::download(
        ctx.client,
        DownloadRequest {
            url: plan.url.clone(),
            template: "{downloads}/{name}".to_string(),
            file_name: Some(plan.asset.clone()),
            downloads_dir: staging.to_path_buf(),
            connections: DOWNLOAD_CONNECTIONS,
            progress: fetch.progress.clone(),
            cancel: fetch.cancel.clone(),
        },
    )
    .await
}

/// A finished download with the right hash, if one is already there. A wrong
/// one is deleted.
async fn finished_download(plan: &Plan, target: &Path) -> Result<Option<PathBuf>, ToolError> {
    if !tokio::fs::try_exists(target).await.unwrap_or(false) {
        return Ok(None);
    }
    if hash_file(plan.tool, target.to_path_buf()).await? == plan.sha256 {
        return Ok(Some(target.to_path_buf()));
    }
    remove_download(target).await;
    Ok(None)
}

/// Download the plan's file into `staging`, resuming what an earlier try left.
/// A saved download that cannot be continued is deleted and started once more
/// from the beginning.
pub(super) async fn fetch(ctx: &InstallCtx<'_>, staging: &Path) -> Result<PathBuf, ToolError> {
    let (plan, tool) = (ctx.plan, ctx.plan.tool);
    let target = staging.join(&plan.asset);
    if let Some(done) = finished_download(plan, &target).await? {
        return Ok(done);
    }

    let (mut resumes, mut restarted) = (0, false);
    loop {
        let resuming = tokio::fs::try_exists(state::state_path(&target))
            .await
            .unwrap_or(false);
        match run_engine(ctx, staging, &target, resuming).await {
            Ok(Outcome::Completed { path, .. }) => return Ok(path),
            Ok(Outcome::Paused {
                reason: PauseReason::Cancelled,
                ..
            }) => return Err(ToolError::Cancelled),
            // The signed link behind github.com expired; asking again gets a new one.
            Ok(Outcome::Paused {
                reason: PauseReason::LinkExpired { status },
                ..
            }) => {
                resumes += 1;
                if resumes > MAX_LINK_RESUMES {
                    return Err(ToolError::network(
                        tool,
                        format!("the download link kept expiring (HTTP {status})"),
                    ));
                }
            }
            Err(e) if cannot_be_continued(&e, resuming) => {
                allow_restart(tool, &mut restarted, &e)?;
                remove_download(&target).await;
            }
            Err(e) => return Err(engine_error(tool, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_that_cannot_be_continued_is_restarted_once_and_then_fails_for_good() {
        let mut restarted = false;
        let first = Error::State("failed to parse state file".into());
        assert!(allow_restart(Tool::Ffmpeg, &mut restarted, &first).is_ok());
        let second = allow_restart(Tool::Ffmpeg, &mut restarted, &Error::FileChanged).unwrap_err();
        assert!(!second.is_transient(), "{second}");
        assert!(
            second.to_string().contains("could not be resumed"),
            "{second}"
        );
    }

    #[test]
    fn only_dead_saved_downloads_are_restarted() {
        assert!(cannot_be_continued(&Error::FileChanged, false));
        assert!(cannot_be_continued(&Error::State("x".into()), true));
        let gone = || Error::Io(std::io::ErrorKind::NotFound.into());
        assert!(cannot_be_continued(&gone(), true));
        assert!(!cannot_be_continued(&gone(), false));
        assert!(!cannot_be_continued(&Error::Protocol("x".into()), true));
        assert!(!cannot_be_continued(&Error::Cancelled, true));
    }
}
