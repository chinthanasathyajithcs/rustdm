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
mod ensure;
pub mod error;
pub mod extract;
mod fetch;
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
use install::ytdlp_exe_name;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
    /// The user asked for the update: ask GitHub now, and a version that was
    /// rolled back from may be installed again.
    UserRequested,
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

/// Lock a mutex even if a panic poisoned it: what these guard stays usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run file work on the blocking pool so it does not hold up the async runtime.
async fn blocking<T: Send + 'static>(
    tool: Tool,
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ToolError::Unusable(format!("work on {tool} stopped unexpectedly: {e}")))?
}

/// Why the last attempt failed or fell back: a message fit for the user, and
/// what went wrong underneath for the log and the command line.
#[derive(Clone, Debug)]
struct Note {
    message: String,
    detail: Option<String>,
}

impl From<&ToolError> for Note {
    fn from(error: &ToolError) -> Self {
        Self {
            message: error.to_string(),
            detail: error.detail().map(str::to_string),
        }
    }
}

/// What the store is doing with one tool, for `status`.
struct Live {
    phase: ToolPhase,
    progress: Option<Arc<Progress>>,
    error: Option<Note>,
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

/// `tools.json` and the lock that makes read-change-write one step. Shared
/// with blocking work, so it lives behind an `Arc`.
struct FileGate {
    file: StoreFile,
    lock: Mutex<()>,
}

impl FileGate {
    fn load(&self, tool: Tool) -> Result<ToolsFile, ToolError> {
        let _guard = lock(&self.lock);
        self.file.load().map_err(io_error(tool))
    }

    /// Read, change and write back one tool's entry.
    fn update(&self, tool: Tool, change: impl FnOnce(&mut ToolEntry)) -> Result<(), ToolError> {
        let _guard = lock(&self.lock);
        let file = self.file.load().map_err(io_error(tool))?;
        let mut entry = file.entry(tool);
        change(&mut entry);
        self.file
            .save(&file.with_entry(tool, entry))
            .map_err(io_error(tool))
    }
}

/// The tools folder: what is installed, how to get more, how to go back.
pub struct ToolStore {
    root: PathBuf,
    client: reqwest::Client,
    sources: Sources,
    files: Arc<FileGate>,
    /// One at a time per tool: a second caller waits, then finds it installed.
    locks: HashMap<Tool, tokio::sync::Mutex<()>>,
    /// Tools whose program file was hashed in this run.
    verified: Mutex<HashSet<Tool>>,
    live: Mutex<HashMap<Tool, Live>>,
    /// Staging names of installs in progress, which cleanup must leave alone.
    active: Arc<Mutex<HashSet<String>>>,
}

/// Removes an install from the in-progress set when it ends, however it ends.
struct ActiveInstall {
    active: Arc<Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for ActiveInstall {
    fn drop(&mut self) {
        lock(&self.active).remove(&self.name);
    }
}

/// Sets a tool back to idle when dropped, so a task that is cancelled or panics
/// cannot leave it looking busy for ever. `note` is what `status` shows after.
struct EndOnDrop<'a> {
    store: &'a ToolStore,
    tool: Tool,
    note: Option<Note>,
}

impl Drop for EndOnDrop<'_> {
    fn drop(&mut self) {
        let note = if std::thread::panicking() {
            Some(Note {
                message: format!("Something went wrong with {}; try again.", self.tool),
                detail: None,
            })
        } else {
            self.note.take()
        };
        self.store.end(self.tool, note);
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
            files: Arc::new(FileGate {
                file: StoreFile::in_dir(&root),
                lock: Mutex::new(()),
            }),
            root,
            client,
            sources,
            locks: Tool::ALL
                .into_iter()
                .map(|t| (t, tokio::sync::Mutex::new(())))
                .collect(),
            verified: Mutex::new(HashSet::new()),
            live: Mutex::new(HashMap::new()),
            active: Arc::new(Mutex::new(HashSet::new())),
        };
        tidy_up(
            &store.root,
            &store.files,
            &store.active,
            &store.pinned_versions(),
        );
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The version of each pinned tool this build would install.
    fn pinned_versions(&self) -> HashMap<Tool, String> {
        self.sources
            .pins
            .iter()
            .map(|(tool, pin)| (*tool, pin.version.clone()))
            .collect()
    }

    /// Remove version and staging folders nothing refers to, off the async runtime.
    async fn tidy(&self) {
        let (root, files, active) = (
            self.root.clone(),
            Arc::clone(&self.files),
            Arc::clone(&self.active),
        );
        let pinned = self.pinned_versions();
        let done = tokio::task::spawn_blocking(move || tidy_up(&root, &files, &active, &pinned));
        if let Err(e) = done.await {
            tracing::warn!("tidying the tools folder stopped unexpectedly: {e}");
        }
    }

    /// Read, change and write back one tool's entry, off the async runtime.
    async fn update_entry(
        &self,
        tool: Tool,
        change: impl FnOnce(&mut ToolEntry) + Send + 'static,
    ) -> Result<(), ToolError> {
        let files = Arc::clone(&self.files);
        blocking(tool, move || files.update(tool, change)).await
    }

    /// The copy of `tool` installed now, if any. Never fetches or updates.
    pub async fn installed(&self, tool: Tool) -> Option<Installed> {
        let entry = self.load_file(tool).ok()?.entry(tool);
        self.current(tool, &entry).await
    }

    fn load_file(&self, tool: Tool) -> Result<ToolsFile, ToolError> {
        self.files.load(tool)
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
        if lock(&self.verified).contains(&tool) {
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
        lock(&self.verified).insert(tool);
        Some(installed)
    }

    fn set_phase(&self, tool: Tool, phase: ToolPhase) {
        lock(&self.live).entry(tool).or_default().phase = phase;
    }

    fn begin(&self, tool: Tool, phase: ToolPhase, progress: Option<Arc<Progress>>) {
        let mut live = lock(&self.live);
        let slot = live.entry(tool).or_default();
        slot.phase = phase;
        slot.progress = progress;
    }

    fn end(&self, tool: Tool, note: Option<Note>) {
        let mut live = lock(&self.live);
        let slot = live.entry(tool).or_default();
        slot.phase = ToolPhase::Idle;
        slot.progress = None;
        slot.error = note;
    }

    fn info(&self, tool: Tool, file: &ToolsFile) -> ToolInfo {
        let entry = file.entry(tool);
        let live = lock(&self.live);
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
        let note = live.and_then(|l| l.error.as_ref());
        ToolInfo {
            tool: tool.into(),
            installed: entry.current,
            previous: entry.previous,
            last_checked_unix: entry.last_check,
            phase: live.map_or(ToolPhase::Idle, |l| l.phase),
            downloaded_bytes: downloaded,
            total_bytes: (total > 0).then_some(total),
            error: note.map(|n| n.message.clone()),
            detail: note.and_then(|n| n.detail.clone()),
        }
    }

    /// Every tool's state, for `tools.status`.
    pub fn status(&self) -> Vec<ToolInfo> {
        let file = self.files.file.load().unwrap_or_default();
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
        let _guard = lock(&self.files.lock);
        let file = self.files.file.load().map_err(io_error(tool))?;
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
            signed_at: entry.previous_signed_at,
            previous_signed_at: entry.signed_at,
            skip_version: entry.current,
            ..entry
        };
        let file = file.with_entry(tool, swapped);
        self.files.file.save(&file).map_err(io_error(tool))?;
        lock(&self.verified).remove(&tool);
        self.end(tool, None);
        Ok(self.info(tool, &file))
    }

    /// Start checking `tool` (installing it if missing), or every installed tool
    /// when `tool` is `None`, in the background. Returns the state at once; the
    /// work shows up in later `status` calls. A tool already being worked on is
    /// left to finish. This is what the user asked for, so a version they rolled
    /// back from may be installed again.
    pub fn start_update(self: &Arc<Self>, tool: Option<Tool>) -> Vec<ToolInfo> {
        let file = self.files.file.load().unwrap_or_default();
        let targets: Vec<Tool> = match tool {
            Some(tool) => vec![tool],
            None => Tool::ALL
                .into_iter()
                .filter(|t| file.entry(*t).current.is_some())
                .collect(),
        };
        for tool in targets {
            {
                let mut live = lock(&self.live);
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
                if let Err(e) = store.ensure(tool, Freshness::UserRequested, ctx).await {
                    tracing::warn!("updating {tool} failed: {e:?}");
                }
            });
        }
        self.status()
    }
}

/// Remove version and staging folders nothing refers to. Best effort.
fn tidy_up(
    root: &Path,
    files: &FileGate,
    active: &Mutex<HashSet<String>>,
    pinned: &HashMap<Tool, String>,
) {
    let _guard = lock(&files.lock);
    match files.file.load() {
        Ok(file) => install::cleanup(
            root,
            &file,
            &install::Keep {
                is_active: &|name| lock(active).contains(name),
                pinned,
            },
        ),
        Err(e) => tracing::warn!("could not read tools.json to tidy up: {e}"),
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

    #[test]
    fn a_dropped_guard_sets_the_tool_idle() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources {
            api_base: String::new(),
            download_base: String::new(),
            ytdlp_repo: String::new(),
            keys: Vec::new(),
            ytdlp_min_version: String::new(),
            ytdlp_asset: None,
            pins: HashMap::new(),
        };
        let store =
            ToolStore::with_sources(dir.path().join("tools"), reqwest::Client::new(), sources)
                .unwrap();
        store.begin(Tool::Deno, ToolPhase::Downloading, None);
        {
            let _guard = EndOnDrop {
                store: &store,
                tool: Tool::Deno,
                note: None,
            };
        }
        let shown = store
            .status()
            .into_iter()
            .find(|i| i.tool == ToolName::Deno);
        assert_eq!(shown.unwrap().phase, ToolPhase::Idle);

        // Even when the work panics, and a poisoned lock does not stop it.
        store.begin(Tool::Deno, ToolPhase::Installing, None);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = EndOnDrop {
                store: &store,
                tool: Tool::Deno,
                note: None,
            };
            let _held = store.live.lock().unwrap();
            panic!("boom");
        }));
        assert!(crashed.is_err());
        let shown = store
            .status()
            .into_iter()
            .find(|i| i.tool == ToolName::Deno);
        let shown = shown.unwrap();
        assert_eq!(shown.phase, ToolPhase::Idle);
        assert!(shown.error.is_some());
    }
}
