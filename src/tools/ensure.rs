//! `ToolStore::ensure`: get a tool installed and up to date, one tool at a time.

use super::install::{self, InstallCtx, Plan};
use super::release::{self, YtDlpRelease, MAX_RATE_LIMIT_WAIT_SECS};
use super::store_file::ToolEntry;
use super::{
    lock, version, ActiveInstall, EndOnDrop, FetchCtx, Freshness, Installed, Note, Tool, ToolError,
    ToolStore, STALE_AFTER_SECS,
};
use crate::ipc::protocol::ToolPhase;

/// A tool that is ready, and why an update did not work, if one did not.
type Ensured = (Installed, Option<Note>);

impl ToolStore {
    /// Make sure `tool` is installed, getting or updating it if needed.
    ///
    /// If an update fails but a copy is installed, that copy is returned and the
    /// reason is shown in `status`; with nothing installed the error is returned.
    pub async fn ensure(
        &self,
        tool: Tool,
        fresh: Freshness,
        ctx: FetchCtx,
    ) -> Result<Installed, ToolError> {
        let _turn = self.locks[&tool].lock().await;
        let mut done = EndOnDrop {
            store: self,
            tool,
            note: None,
        };
        let result = self.ensure_locked(tool, fresh, &ctx).await;
        match &result {
            Ok((_, note)) => done.note = note.clone(),
            Err(ToolError::Cancelled) => {}
            Err(e) => {
                tracing::warn!("{tool} could not be installed: {e:?}");
                done.note = Some(Note::from(e));
            }
        }
        result.map(|(installed, _)| installed)
    }

    async fn ensure_locked(
        &self,
        tool: Tool,
        fresh: Freshness,
        ctx: &FetchCtx,
    ) -> Result<Ensured, ToolError> {
        let entry = self.load_file(tool)?.entry(tool);
        let installed = self.current(tool, &entry).await;
        let outcome = match tool {
            Tool::YtDlp => {
                self.ensure_ytdlp(&entry, installed.clone(), fresh, ctx)
                    .await
            }
            _ => {
                self.ensure_pinned(tool, &entry, installed.clone(), fresh, ctx)
                    .await
            }
        };
        match (outcome, installed) {
            (Err(ToolError::Cancelled), _) => Err(ToolError::Cancelled),
            // An update that did not work leaves the working copy in use.
            (Err(e), Some(installed)) => {
                tracing::warn!("keeping {tool} {}: {e:?}", installed.version);
                Ok((installed, Some(Note::from(&e))))
            }
            (outcome, _) => outcome,
        }
    }

    /// ffmpeg and deno: the build this rustdm ships, installed when missing or
    /// older. A version the user rolled back from stays rolled back, unless they
    /// ask for the update themselves.
    async fn ensure_pinned(
        &self,
        tool: Tool,
        entry: &ToolEntry,
        installed: Option<Installed>,
        fresh: Freshness,
        ctx: &FetchCtx,
    ) -> Result<Ensured, ToolError> {
        let pin = self.sources.pins.get(&tool).ok_or_else(|| {
            ToolError::Unusable(format!("{tool} isn't available for this system"))
        })?;
        if let Some(installed) = &installed {
            let up_to_date = !version::is_newer(&pin.version, &installed.version);
            let rolled_back_from = entry.skip_version.as_deref() == Some(pin.version.as_str())
                && fresh != Freshness::UserRequested;
            if up_to_date || rolled_back_from {
                return Ok((installed.clone(), None));
            }
        }
        let plan = Plan::for_pin(tool, pin, &self.sources.download_base)?;
        let kept = installed.map(|i| i.version);
        Ok((self.run_install(plan, kept, ctx).await?, None))
    }

    async fn ensure_ytdlp(
        &self,
        entry: &ToolEntry,
        installed: Option<Installed>,
        fresh: Freshness,
        ctx: &FetchCtx,
    ) -> Result<Ensured, ToolError> {
        let now = release::now_unix();
        if let Some(current) = &installed {
            if !needs_check(entry, fresh, now) {
                return Ok((current.clone(), None));
            }
        }
        // Do not ask again while GitHub has said to wait.
        if let Some(limited) = rate_limit_hold(entry, installed.as_ref(), now) {
            return match installed {
                Some(current) => Ok((current, Some(Note::from(&limited)))),
                None => Err(limited),
            };
        }

        let release = self.look_up_ytdlp(installed.as_ref(), now).await?;
        let Some(current) = installed else {
            let plan = Plan::for_ytdlp(&release);
            return Ok((self.run_install(plan, None, ctx).await?, None));
        };
        if !newer_release_wanted(entry, &current, &release, fresh)? {
            return Ok((current, None));
        }
        let plan = Plan::for_ytdlp(&release);
        let kept = Some(current.version);
        Ok((self.run_install(plan, kept, ctx).await?, None))
    }

    /// Ask GitHub for the newest yt-dlp, and note when we asked. A rate limit is
    /// remembered so nothing asks again before it ends.
    async fn look_up_ytdlp(
        &self,
        installed: Option<&Installed>,
        now: u64,
    ) -> Result<YtDlpRelease, ToolError> {
        let tool = Tool::YtDlp;
        self.begin(tool, ToolPhase::Checking, None);
        let release = match release::latest_ytdlp(&self.client, &self.sources).await {
            Ok(release) => release,
            Err(ToolError::RateLimited { until_unix, .. }) => {
                self.update_entry(tool, move |e| e.retry_not_before = until_unix)
                    .await?;
                return Err(ToolError::RateLimited {
                    tool,
                    until_unix,
                    installed: installed.map(|i| i.version.clone()),
                });
            }
            Err(e) => return Err(e),
        };
        self.update_entry(tool, move |e| {
            e.last_check = Some(now);
            e.retry_not_before = None;
        })
        .await?;
        Ok(release)
    }

    /// Download and put `plan` in place, then record it in `tools.json`.
    /// `kept` is the working version it replaces, which becomes `previous`.
    async fn run_install(
        &self,
        plan: Plan,
        kept: Option<String>,
        ctx: &FetchCtx,
    ) -> Result<Installed, ToolError> {
        let tool = plan.tool;
        let name = plan.staging_name();
        lock(&self.active).insert(name.clone());
        let _in_progress = ActiveInstall {
            active: self.active.clone(),
            name,
        };

        self.begin(tool, ToolPhase::Downloading, Some(ctx.progress.clone()));
        let finished = install::install(&InstallCtx {
            root: &self.root,
            client: &self.client,
            plan: &plan,
            fetch: ctx,
            kept: kept.as_deref(),
            on_phase: &|phase| self.set_phase(tool, phase),
        })
        .await?;

        let (version, sha256, signed_at) =
            (finished.version.clone(), finished.sha256, plan.signed_at);
        self.update_entry(tool, move |entry| {
            if let Some(old) = kept.filter(|old| *old != version) {
                entry.previous = Some(old);
                entry.previous_sha256 = entry.sha256.take();
                entry.previous_signed_at = entry.signed_at.take();
            }
            // A version that was rolled back from, installed again on purpose.
            if entry.skip_version.as_deref() == Some(version.as_str()) {
                entry.skip_version = None;
            }
            entry.current = Some(version);
            entry.sha256 = Some(sha256);
            entry.signed_at = signed_at.or(entry.signed_at);
        })
        .await?;
        lock(&self.verified).insert(tool);
        self.tidy().await;
        self.installed_at(tool, &finished.version)
            .ok_or_else(|| ToolError::Unusable(format!("{tool} isn't available for this system")))
    }
}

/// Whether an installed yt-dlp is to be checked against GitHub, given how fresh
/// the caller needs it.
fn needs_check(entry: &ToolEntry, fresh: Freshness, now: u64) -> bool {
    let stale = entry
        .last_check
        .is_none_or(|at| now.saturating_sub(at) > STALE_AFTER_SECS);
    match fresh {
        Freshness::AsInstalled => false,
        Freshness::CheckIfStale => stale,
        Freshness::CheckNow | Freshness::UserRequested => true,
    }
}

/// The error to give while GitHub has told us to wait, if it has. A wait
/// stored as more than an hour away is not one we ever stored (or the clock
/// moved back), so it does not hold anything up.
fn rate_limit_hold(
    entry: &ToolEntry,
    installed: Option<&Installed>,
    now: u64,
) -> Option<ToolError> {
    let latest = now.saturating_add(MAX_RATE_LIMIT_WAIT_SECS);
    let until = entry
        .retry_not_before
        .filter(|until| (now.saturating_add(1)..=latest).contains(until))?;
    Some(ToolError::RateLimited {
        tool: Tool::YtDlp,
        until_unix: Some(until),
        installed: installed.map(|i| i.version.clone()),
    })
}

/// Whether `release` should replace `current`. It must be newer, must not be a
/// version that was rolled back from (unless the user asked), and its checksum
/// list must have been signed after the installed version's: an old signed
/// release offered under a new tag cannot pass.
fn newer_release_wanted(
    entry: &ToolEntry,
    current: &Installed,
    release: &YtDlpRelease,
    fresh: Freshness,
) -> Result<bool, ToolError> {
    if !version::is_newer(&release.version, &current.version) {
        return Ok(false);
    }
    let rolled_back_from = entry.skip_version.as_deref() == Some(release.version.as_str());
    if rolled_back_from && fresh != Freshness::UserRequested {
        return Ok(false);
    }
    if entry
        .signed_at
        .is_some_and(|stored| release.signed_at <= stored)
    {
        return Err(ToolError::Unusable(format!(
            "yt-dlp {} was signed no later than the installed {}; ignoring it",
            release.version, current.version
        )));
    }
    Ok(true)
}
