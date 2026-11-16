//! `tools.json`: what is installed, kept next to the tool folders.

use super::Tool;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub const TOOLS_FILE_VERSION: u32 = 1;
pub const TOOLS_FILE_NAME: &str = "tools.json";

/// What is known about one tool. Every field may be missing in an older file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntry {
    /// Installed version, which is also its folder name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// The version before `current`, kept so `rollback` has somewhere to go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// SHA-256 of the current program file, checked once per daemon run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sha256: Option<String>,
    /// When the release list was last asked, in unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<u64>,
    /// A version that was rolled back from; never installed again by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_version: Option<String>,
    /// Do not ask GitHub again before this time (rate limit), in unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_not_before: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsFile {
    pub version: u32,
    #[serde(default)]
    pub tools: BTreeMap<Tool, ToolEntry>,
}

impl Default for ToolsFile {
    fn default() -> Self {
        Self {
            version: TOOLS_FILE_VERSION,
            tools: BTreeMap::new(),
        }
    }
}

impl ToolsFile {
    pub fn entry(&self, tool: Tool) -> ToolEntry {
        self.tools.get(&tool).cloned().unwrap_or_default()
    }

    /// A copy with `tool`'s entry replaced.
    pub fn with_entry(&self, tool: Tool, entry: ToolEntry) -> ToolsFile {
        let mut tools = self.tools.clone();
        tools.insert(tool, entry);
        ToolsFile {
            version: self.version,
            tools,
        }
    }
}

/// Reads and writes `tools.json`.
pub struct StoreFile {
    path: PathBuf,
}

impl StoreFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self::new(dir.join(TOOLS_FILE_NAME))
    }

    /// Load the file. A missing file gives an empty one. A file that cannot be
    /// parsed is renamed to `tools.json.corrupt` (so it can be looked at) and
    /// also gives an empty one: nothing is treated as installed.
    pub fn load(&self) -> io::Result<ToolsFile> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ToolsFile::default()),
            Err(e) => return Err(e),
        };
        let problem = match serde_json::from_slice::<ToolsFile>(&bytes) {
            Ok(file) if file.version == TOOLS_FILE_VERSION => return Ok(file),
            Ok(file) => format!("unknown tools file version {}", file.version),
            Err(e) => e.to_string(),
        };
        let mut aside = self.path.clone().into_os_string();
        aside.push(".corrupt");
        let aside = PathBuf::from(aside);
        std::fs::rename(&self.path, &aside)?;
        tracing::warn!(
            "could not read {} ({problem}); moved it to {} and treating no tools as installed",
            self.path.display(),
            aside.display()
        );
        Ok(ToolsFile::default())
    }

    pub fn save(&self, file: &ToolsFile) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::state::write_atomic(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ToolsFile {
        ToolsFile::default().with_entry(
            Tool::YtDlp,
            ToolEntry {
                current: Some("2026.11.14.232819".into()),
                previous: Some("2026.11.10.1".into()),
                sha256: Some("a".repeat(64)),
                previous_sha256: Some("b".repeat(64)),
                last_check: Some(1_794_000_000),
                skip_version: Some("2026.11.12".into()),
                retry_not_before: None,
            },
        )
    }

    #[test]
    fn the_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreFile::in_dir(dir.path());
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), sample());
    }

    #[test]
    fn a_missing_file_means_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = StoreFile::in_dir(dir.path()).load().unwrap();
        assert_eq!(loaded, ToolsFile::default());
        assert_eq!(loaded.entry(Tool::Deno), ToolEntry::default());
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreFile::in_dir(dir.path());
        std::fs::write(dir.path().join(TOOLS_FILE_NAME), b"{not json").unwrap();

        assert_eq!(store.load().unwrap(), ToolsFile::default());
        assert!(!dir.path().join(TOOLS_FILE_NAME).exists());
        assert_eq!(
            std::fs::read(dir.path().join("tools.json.corrupt")).unwrap(),
            b"{not json"
        );
        // A later save works and loads normally.
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), sample());
    }

    #[test]
    fn an_unknown_version_is_set_aside_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(TOOLS_FILE_NAME),
            br#"{"version": 99, "tools": {}}"#,
        )
        .unwrap();
        let loaded = StoreFile::in_dir(dir.path()).load().unwrap();
        assert_eq!(loaded, ToolsFile::default());
        assert!(dir.path().join("tools.json.corrupt").exists());
    }

    #[test]
    fn fields_missing_from_an_older_file_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(TOOLS_FILE_NAME),
            br#"{"version": 1, "tools": {"ffmpeg": {"current": "8.1.3"}}}"#,
        )
        .unwrap();
        let loaded = StoreFile::in_dir(dir.path()).load().unwrap();
        let entry = loaded.entry(Tool::Ffmpeg);
        assert_eq!(entry.current.as_deref(), Some("8.1.3"));
        assert_eq!(entry.previous, None);
    }
}
