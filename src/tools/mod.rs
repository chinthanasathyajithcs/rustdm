//! Fetches, verifies, updates and rolls back the helper programs rustdm needs
//! for video: yt-dlp, ffmpeg and deno. Nothing happens until something asks
//! for a tool.
//!
//! Contract for code that runs the tools: yt-dlp is started with
//! `--ignore-config --no-js-runtimes --js-runtimes deno:<deno exe>
//! --ffmpeg-location <ffmpeg dir>` and never with `-U`; every update goes
//! through `ToolStore`.

pub mod checksum;
pub mod cli;
pub mod display;
pub mod error;
pub mod extract;
pub mod install;
pub mod openpgp;
pub mod pins;
pub mod release;
pub mod store_file;
pub mod version;

pub use error::ToolError;
pub use release::Sources;

use crate::engine::Progress;
use crate::ipc::protocol::{ToolInfo, ToolName, ToolPhase};
use install::{ytdlp_exe_name, Plan};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use store_file::{StoreFile, ToolEntry, ToolsFile};
use tokio::sync::watch;

/// A yt-dlp release list checked longer ago than this is checked again on next use.
pub const STALE_AFTER_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno];

    /// The name people know it by; also its folder name under `tools/`.
    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Deno => "deno",
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl From<ToolName> for Tool {
    fn from(name: ToolName) -> Self {
        match name {
            ToolName::YtDlp => Tool::YtDlp,
            ToolName::Ffmpeg => Tool::Ffmpeg,
            ToolName::Deno => Tool::Deno,
        }
    }
}

impl From<Tool> for ToolName {
    fn from(tool: Tool) -> Self {
        match tool {
            Tool::YtDlp => ToolName::YtDlp,
            Tool::Ffmpeg => ToolName::Ffmpeg,
            Tool::Deno => ToolName::Deno,
        }
    }
}

/// A tool that is installed and ready to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub tool: Tool,
    pub version: String,
    /// The folder holding the program (what `--ffmpeg-location` wants for ffmpeg).
    pub dir: PathBuf,
    pub exe: PathBuf,
}

/// How sure the caller needs to be that a tool is up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Whatever is installed will do.
    AsInstalled,
    /// Ask GitHub if the last check was over 24 hours ago.
    CheckIfStale,
    /// Ask GitHub now.
    CheckNow,
}

/// What a caller lends to a fetch: the progress counters to count into (their
/// rate limiters apply, so speed caps hold) and a way to cancel.
pub struct FetchCtx {
    pub progress: Arc<Progress>,
    pub cancel: Option<watch::Receiver<bool>>,
}

impl FetchCtx {
    pub fn new() -> Self {
        Self {
            progress: Arc::new(Progress::new()),
            cancel: None,
        }
    }
}

impl Default for FetchCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// What the store is doing with one tool, for `status`.
struct Live {
    phase: ToolPhase,
    progress: Option<Arc<Progress>>,
    /// Why the last attempt failed or fell back, in words fit for the user.
    error: Option<String>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            phase: ToolPhase::Idle,
            progress: None,
            error: None,
        }
    }
}

/// The tools folder: what is installed, how to get more, how to go back.
pub struct ToolStore {
    root: PathBuf,
    client: reqwest::Client,
    sources: Sources,
    file: StoreFile,
    /// Held while `tools.json` is read, changed and written back.
    file_lock: Mutex<()>,
    /// One at a time per tool: a second caller waits, then finds it installed.
    locks: HashMap<Tool, tokio::sync::Mutex<()>>,
    /// Tools whose program file was hashed in this run.
    verified: Mutex<HashSet<Tool>>,
    live: Mutex<HashMap<Tool, Live>>,
    /// Staging names of installs in progress, which cleanup must leave alone.
    active: Mutex<HashSet<String>>,
}

/// Removes an install from the in-progress set when it ends, however it ends.
struct ActiveInstall<'a> {
    store: &'a ToolStore,
    name: String,
}

impl Drop for ActiveInstall<'_> {
    fn drop(&mut self) {
        self.store.active.lock().unwrap().remove(&self.name);
    }
}

fn io_error(tool: Tool) -> impl Fn(std::io::Error) -> ToolError {
    move |source| ToolError::Io { tool, source }
}

impl ToolStore {
    /// The tools folder at `root`, fetching from GitHub.
    pub fn open(root: PathBuf, client: reqwest::Client) -> std::io::Result<Self> {
        Self::with_sources(root, client, Sources::production()?)
    }

    /// Like `open`, but fetching from, and trusting, what `sources` says (for tests).
    pub fn with_sources(
        root: PathBuf,
        client: reqwest::Client,
        sources: Sources,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let store = Self {
            file: StoreFile::in_dir(&root),
            root,
            client,
            sources,
            file_lock: Mutex::new(()),
            locks: Tool::ALL
                .into_iter()
                .map(|t| (t, tokio::sync::Mutex::new(())))
                .collect(),
            verified: Mutex::new(HashSet::new()),
            live: Mutex::new(HashMap::new()),
            active: Mutex::new(HashSet::new()),
        };
        store.tidy();
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Remove version and staging folders nothing refers to. Best effort.
    fn tidy(&self) {
        let _guard = self.file_lock.lock().unwrap();
        match self.file.load() {
            Ok(file) => install::cleanup(&self.root, &file, &self.active.lock().unwrap()),
            Err(e) => tracing::warn!("could not read tools.json to tidy up: {e}"),
        }
    }

    /// Read, change and write back one tool's entry.
    fn update_entry(
        &self,
        tool: Tool,
        change: impl FnOnce(&mut ToolEntry),
    ) -> Result<(), ToolError> {
        let _guard = self.file_lock.lock().unwrap();
        let file = self.file.load().map_err(io_error(tool))?;
        let mut entry = file.entry(tool);
        change(&mut entry);
        self.file
            .save(&file.with_entry(tool, entry))
            .map_err(io_error(tool))
    }

    fn load_file(&self, tool: Tool) -> Result<ToolsFile, ToolError> {
        let _guard = self.file_lock.lock().unwrap();
        self.file.load().map_err(io_error(tool))
    }

    /// Where `tool`'s program sits inside a version folder: (file, folder), both relative.
    fn layout(&self, tool: Tool) -> Option<(String, String)> {
        match tool {
            Tool::YtDlp => Some((ytdlp_exe_name().to_string(), String::new())),
            _ => self
                .sources
                .pins
                .get(&tool)
                .map(|pin| (pin.exe.clone(), pin.exe_dir.clone())),
        }
    }

    fn installed_at(&self, tool: Tool, version: &str) -> Option<Installed> {
        let (exe, exe_dir) = self.layout(tool)?;
        let root = install::version_dir(&self.root, tool, version);
        Some(Installed {
            tool,
            version: version.to_string(),
            dir: root.join(exe_dir),
            exe: root.join(exe),
        })
    }

    /// The installed copy of `tool`, if it is there and is the file we saved.
    /// The program file is hashed once per run, so a replaced or damaged file
    /// is noticed and reinstalled.
    async fn current(&self, tool: Tool, entry: &ToolEntry) -> Option<Installed> {
        let installed = self.installed_at(tool, entry.current.as_deref()?)?;
        if !installed.exe.is_file() {
            tracing::warn!(
                "{tool} {} is missing from disk; reinstalling",
                installed.version
            );
            return None;
        }
        if self.verified.lock().unwrap().contains(&tool) {
            return Some(installed);
        }
        if let Some(expected) = &entry.sha256 {
            let path = installed.exe.clone();
            let actual = tokio::task::spawn_blocking(move || checksum::sha256_file(&path)).await;
            if !matches!(actual, Ok(Ok(ref hash)) if hash == expected) {
                tracing::warn!(
                    "{tool} {} does not match what was installed; reinstalling",
                    installed.version
                );
                return None;
            }
        }
        self.verified.lock().unwrap().insert(tool);
        Some(installed)
    }

    fn set_phase(&self, tool: Tool, phase: ToolPhase) {
        self.live.lock().unwrap().entry(tool).or_default().phase = phase;
    }

    fn begin(&self, tool: Tool, phase: ToolPhase, progress: Option<Arc<Progress>>) {
        let mut live = self.live.lock().unwrap();
        let slot = live.entry(tool).or_default();
        slot.phase = phase;
        slot.progress = progress;
    }

    fn end(&self, tool: Tool, note: Option<String>) {
        let mut live = self.live.lock().unwrap();
        let slot = live.entry(tool).or_default();
        slot.phase = ToolPhase::Idle;
        slot.progress = None;
        slot.error = note;
    }

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
        let result = self.ensure_locked(tool, fresh, &ctx).await;
        match &result {
            Ok((_, note)) => self.end(tool, note.clone()),
            Err(ToolError::Cancelled) => self.end(tool, None),
            Err(e) => self.end(tool, Some(e.to_string())),
        }
        result.map(|(installed, _)| installed)
    }

    async fn ensure_locked(
        &self,
        tool: Tool,
        fresh: Freshness,
        ctx: &FetchCtx,
    ) -> Result<(Installed, Option<String>), ToolError> {
        let entry = self.load_file(tool)?.entry(tool);
        let installed = self.current(tool, &entry).await;
        let outcome = match tool {
            Tool::YtDlp => {
                self.ensure_ytdlp(&entry, installed.clone(), fresh, ctx)
                    .await
            }
            _ => self.ensure_pinned(tool, installed.clone(), ctx).await,
        };
        match (outcome, installed) {
            (Err(ToolError::Cancelled), _) => Err(ToolError::Cancelled),
            // An update that did not work leaves the working copy in use.
            (Err(e), Some(installed)) => {
                tracing::warn!("keeping {tool} {}: {e}", installed.version);
                Ok((installed, Some(e.to_string())))
            }
            (outcome, _) => outcome,
        }
    }

    async fn ensure_pinned(
        &self,
        tool: Tool,
        installed: Option<Installed>,
        ctx: &FetchCtx,
    ) -> Result<(Installed, Option<String>), ToolError> {
        let pin = self.sources.pins.get(&tool).ok_or_else(|| {
            ToolError::Unusable(format!("{tool} isn't available for this system"))
        })?;
        if let Some(installed) = &installed {
            if !version::is_newer(&pin.version, &installed.version) {
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
    ) -> Result<(Installed, Option<String>), ToolError> {
        let tool = Tool::YtDlp;
        let now = release::now_unix();

        if let Some(current) = &installed {
            let stale = entry
                .last_check
                .is_none_or(|at| now.saturating_sub(at) > STALE_AFTER_SECS);
            let skip_check = match fresh {
                Freshness::AsInstalled => true,
                Freshness::CheckIfStale => !stale,
                Freshness::CheckNow => false,
            };
            if skip_check {
                return Ok((current.clone(), None));
            }
        }
        // Do not ask again while GitHub has said to wait.
        if let Some(until) = entry.retry_not_before.filter(|until| *until > now) {
            let limited = ToolError::RateLimited {
                tool,
                until_unix: Some(until),
                installed: installed.as_ref().map(|i| i.version.clone()),
            };
            return match installed {
                Some(current) => Ok((current, Some(limited.to_string()))),
                None => Err(limited),
            };
        }

        self.begin(tool, ToolPhase::Checking, None);
        let release = match release::latest_ytdlp(&self.client, &self.sources).await {
            Ok(release) => release,
            Err(ToolError::RateLimited { until_unix, .. }) => {
                self.update_entry(tool, |e| e.retry_not_before = until_unix)?;
                return Err(ToolError::RateLimited {
                    tool,
                    until_unix,
                    installed: installed.as_ref().map(|i| i.version.clone()),
                });
            }
            Err(e) => return Err(e),
        };
        self.update_entry(tool, |e| {
            e.last_check = Some(now);
            e.retry_not_before = None;
        })?;

        let wanted = match &installed {
            Some(current) => {
                version::is_newer(&release.version, &current.version)
                    && entry.skip_version.as_deref() != Some(release.version.as_str())
            }
            None => true,
        };
        match installed {
            Some(current) if !wanted => Ok((current, None)),
            current => {
                let kept = current.map(|c| c.version);
                let plan = Plan::for_ytdlp(&release);
                Ok((self.run_install(plan, kept, ctx).await?, None))
            }
        }
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
        self.active.lock().unwrap().insert(name.clone());
        let _in_progress = ActiveInstall { store: self, name };

        self.begin(tool, ToolPhase::Downloading, Some(ctx.progress.clone()));
        let finished = install::install(
            &self.root,
            &self.client,
            &plan,
            ctx,
            kept.as_deref(),
            &|phase| self.set_phase(tool, phase),
        )
        .await?;

        let version = finished.version.clone();
        self.update_entry(tool, |entry| {
            if let Some(old) = kept.filter(|old| *old != version) {
                entry.previous = Some(old);
                entry.previous_sha256 = entry.sha256.take();
            }
            entry.current = Some(version.clone());
            entry.sha256 = Some(finished.sha256.clone());
        })?;
        self.verified.lock().unwrap().insert(tool);
        self.tidy();
        self.installed_at(tool, &finished.version)
            .ok_or_else(|| ToolError::Unusable(format!("{tool} isn't available for this system")))
    }

    fn info(&self, tool: Tool, file: &ToolsFile) -> ToolInfo {
        let entry = file.entry(tool);
        let live = self.live.lock().unwrap();
        let live = live.get(&tool);
        let (downloaded, total) = live
            .and_then(|l| l.progress.as_ref())
            .map(|p| {
                (
                    p.downloaded.load(Ordering::Relaxed),
                    p.total.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0));
        ToolInfo {
            tool: tool.into(),
            installed: entry.current,
            previous: entry.previous,
            last_checked_unix: entry.last_check,
            phase: live.map_or(ToolPhase::Idle, |l| l.phase),
            downloaded_bytes: downloaded,
            total_bytes: (total > 0).then_some(total),
            error: live.and_then(|l| l.error.clone()),
        }
    }

    /// Every tool's state, for `tools.status`.
    pub fn status(&self) -> Vec<ToolInfo> {
        let file = self.file.load().unwrap_or_default();
        Tool::ALL
            .iter()
            .map(|&tool| self.info(tool, &file))
            .collect()
    }

    /// Go back to the previous version of `tool`. The version rolled back from is
    /// not installed again by an automatic update.
    pub fn rollback(&self, tool: Tool) -> Result<ToolInfo, ToolError> {
        let _turn = self.locks[&tool].try_lock().map_err(|_| {
            ToolError::Unusable(format!("{tool} is being updated; try again in a moment."))
        })?;
        let _guard = self.file_lock.lock().unwrap();
        let file = self.file.load().map_err(io_error(tool))?;
        let entry = file.entry(tool);
        let Some(previous) = entry.previous.clone() else {
            return Err(ToolError::Unusable(format!(
                "There is no earlier version of {tool} to go back to."
            )));
        };
        let on_disk = self
            .installed_at(tool, &previous)
            .is_some_and(|i| i.exe.is_file());
        if !on_disk {
            return Err(ToolError::Unusable(format!(
                "The earlier version of {tool} ({previous}) is no longer on disk."
            )));
        }
        let swapped = ToolEntry {
            current: Some(previous),
            previous: entry.current.clone(),
            sha256: entry.previous_sha256.clone(),
            previous_sha256: entry.sha256.clone(),
            skip_version: entry.current,
            ..entry
        };
        let file = file.with_entry(tool, swapped);
        self.file.save(&file).map_err(io_error(tool))?;
        self.verified.lock().unwrap().remove(&tool);
        self.end(tool, None);
        Ok(self.info(tool, &file))
    }

    /// Start checking `tool` (installing it if missing), or every installed tool
    /// when `tool` is `None`, in the background. Returns the state at once; the
    /// work shows up in later `status` calls. A tool already being worked on is
    /// left to finish.
    pub fn start_update(self: &Arc<Self>, tool: Option<Tool>) -> Vec<ToolInfo> {
        let file = self.file.load().unwrap_or_default();
        let targets: Vec<Tool> = match tool {
            Some(tool) => vec![tool],
            None => Tool::ALL
                .into_iter()
                .filter(|t| file.entry(*t).current.is_some())
                .collect(),
        };
        for tool in targets {
            {
                let mut live = self.live.lock().unwrap();
                let slot = live.entry(tool).or_default();
                if slot.phase != ToolPhase::Idle {
                    continue;
                }
                slot.phase = ToolPhase::Checking;
                slot.error = None;
            }
            let store = Arc::clone(self);
            tokio::spawn(async move {
                let ctx = FetchCtx::new();
                if let Err(e) = store.ensure(tool, Freshness::CheckNow, ctx).await {
                    tracing::warn!("updating {tool} failed: {e}");
                }
            });
        }
        self.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_serialize_in_kebab_case() {
        assert_eq!(serde_json::to_string(&Tool::YtDlp).unwrap(), "\"yt-dlp\"");
        assert_eq!(serde_json::to_string(&Tool::Deno).unwrap(), "\"deno\"");
        assert_eq!(Tool::Ffmpeg.to_string(), "ffmpeg");
    }

    #[test]
    fn protocol_names_convert_both_ways() {
        for tool in Tool::ALL {
            assert_eq!(Tool::from(ToolName::from(tool)), tool);
        }
        assert_eq!(
            serde_json::to_string(&ToolName::YtDlp).unwrap(),
            serde_json::to_string(&Tool::YtDlp).unwrap()
        );
    }
}
