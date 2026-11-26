use crate::ipc::protocol::{AddedBy, DaemonLimits, DownloadStatus, SavedDefaults};
use crate::video::VideoPlan;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const QUEUE_VERSION: u32 = 1;
pub const QUEUE_FILE_NAME: &str = "queue.json";
/// Environment variable that overrides the default data folder.
pub const DATA_DIR_ENV: &str = "RUSTDM_DATA_DIR";

/// Everything the daemon needs to rebuild its queue after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub version: u32,
    pub next_id: u64,
    pub limits: DaemonLimits,
    /// Defaults for new downloads. Missing from older queue files.
    #[serde(default)]
    pub defaults: SavedDefaults,
    pub downloads: Vec<SavedDownload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDownload {
    pub id: String,
    pub url: String,
    /// Folder the download saves into.
    pub directory: PathBuf,
    /// File name the user asked for, if any.
    pub requested_name: Option<String>,
    pub filename: String,
    /// Final file path once the engine has chosen it.
    pub output: Option<PathBuf>,
    pub connections: u32,
    pub status: DownloadStatus,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub error_message: Option<String>,
    /// This download's own speed cap; 0 means none. Missing from older queue files.
    #[serde(default)]
    pub max_bytes_per_sec: u64,
    /// The download came with browser cookies, kept apart in the secret store.
    /// Missing from older queue files.
    #[serde(default)]
    pub has_cookies: bool,
    /// Page the download came from, sent as the Referer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    /// The browser's User-Agent, sent with every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// What is known of a video download. Missing from older queue files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoPlan>,
    /// The quality the user asked for, as they wrote it. Missing from older queue files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Who added the download, when it was the browser. Missing from older queue files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<AddedBy>,
    /// The title of the page the link came from. Missing from older queue files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_title: Option<String>,
}

/// Reads and writes the queue file. Saves are serialized and atomic.
pub struct QueueStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl QueueStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self::new(dir.join(QUEUE_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the saved queue. A missing file gives `None`. A file that cannot be
    /// parsed is renamed to `<name>.corrupt` and also gives `None`, so it is kept
    /// for inspection instead of being overwritten.
    pub fn load(&self) -> io::Result<Option<QueueSnapshot>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        let problem = match serde_json::from_slice::<QueueSnapshot>(&bytes) {
            Ok(snapshot) if snapshot.version == QUEUE_VERSION => return Ok(Some(snapshot)),
            Ok(snapshot) => format!("unknown queue version {}", snapshot.version),
            Err(e) => e.to_string(),
        };

        let aside = self.corrupt_path();
        std::fs::rename(&self.path, &aside)?;
        tracing::warn!(
            "could not read download queue {} ({problem}); moved it to {} and starting empty",
            self.path.display(),
            aside.display()
        );
        Ok(None)
    }

    pub fn save(&self, snapshot: &QueueSnapshot) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(snapshot).map_err(io::Error::other)?;
        let _guard = self.write_lock.lock().unwrap();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::state::write_atomic(&self.path, &json)
    }

    fn corrupt_path(&self) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(".corrupt");
        PathBuf::from(name)
    }
}

/// Default folder for the daemon's queue file:
/// `%LOCALAPPDATA%\rustdm` on Windows, `$XDG_STATE_HOME/rustdm` or
/// `~/.local/state/rustdm` elsewhere.
pub fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("rustdm");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(state_home).join("rustdm");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/state/rustdm");
        }
    }
    std::env::temp_dir().join("rustdm")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> QueueSnapshot {
        QueueSnapshot {
            version: QUEUE_VERSION,
            next_id: 2,
            limits: DaemonLimits {
                max_active: 2,
                max_bytes_per_sec: 0,
            },
            defaults: SavedDefaults::default(),
            downloads: vec![SavedDownload {
                id: "dl_2".to_string(),
                url: "http://example.com/a.iso".to_string(),
                directory: PathBuf::from("/downloads"),
                requested_name: None,
                filename: "a.iso".to_string(),
                output: Some(PathBuf::from("/downloads/a.iso")),
                connections: 8,
                status: DownloadStatus::Queued,
                total_bytes: Some(1000),
                downloaded_bytes: 400,
                error_message: None,
                max_bytes_per_sec: 0,
                has_cookies: true,
                referer: Some("https://example.com/files".to_string()),
                user_agent: Some("Mozilla/5.0".to_string()),
                video: None,
                quality: None,
                added_by: None,
                page_title: None,
            }],
        }
    }

    #[test]
    fn old_queue_files_without_video_load() {
        let mut json = serde_json::to_value(sample()).unwrap();
        let saved = json["downloads"][0].as_object_mut().unwrap();
        saved.remove("video");
        saved.remove("quality");
        let snapshot: QueueSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snapshot.downloads[0].video, None);
        assert_eq!(snapshot.downloads[0].quality, None);
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(!text.contains("\"video\"") && !text.contains("\"quality\""));
    }

    #[test]
    fn missing_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(QueueStore::in_dir(dir.path()).load().unwrap(), None);
    }

    #[test]
    fn saved_queue_loads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(&dir.path().join("nested"));
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), Some(sample()));
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(dir.path());
        std::fs::write(store.path(), b"{ not json").unwrap();

        assert_eq!(store.load().unwrap(), None);
        assert!(!store.path().exists());
        assert_eq!(
            std::fs::read(dir.path().join("queue.json.corrupt")).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn a_saved_download_without_request_context_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(dir.path());
        std::fs::write(
            store.path(),
            br#"{"version":1,"next_id":1,"limits":{"max_active":3,"max_bytes_per_sec":0},
                "downloads":[{"id":"dl_1","url":"http://example.com/a.iso","directory":"/d",
                "requested_name":null,"filename":"a.iso","output":null,"connections":8,
                "status":"queued","total_bytes":null,"downloaded_bytes":0,"error_message":null}]}"#,
        )
        .unwrap();
        let saved = &store.load().unwrap().expect("loads").downloads[0];
        assert!(!saved.has_cookies);
        assert_eq!(saved.referer, None);
        assert_eq!(saved.user_agent, None);
    }

    #[test]
    fn the_queue_file_records_that_cookies_exist_but_never_what_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(dir.path());
        store.save(&sample()).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"has_cookies\": true"));
        assert!(!text.contains("\"cookies\""));
    }

    #[test]
    fn a_queue_file_without_defaults_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(dir.path());
        std::fs::write(
            store.path(),
            br#"{"version":1,"next_id":0,"limits":{"max_active":3,"max_bytes_per_sec":0},"downloads":[]}"#,
        )
        .unwrap();
        let snapshot = store.load().unwrap().expect("loads");
        assert_eq!(snapshot.defaults, SavedDefaults::default());
    }

    #[test]
    fn an_old_queue_file_without_added_by_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::in_dir(dir.path());
        std::fs::write(
            store.path(),
            br#"{"version":1,"next_id":1,"limits":{"max_active":3,"max_bytes_per_sec":0},
                "downloads":[{"id":"dl_1","url":"http://example.com/a.iso","directory":"/d",
                "requested_name":null,"filename":"a.iso","output":null,"connections":8,
                "status":"queued","total_bytes":null,"downloaded_bytes":0,"error_message":null}]}"#,
        )
        .unwrap();
        let mut snapshot = store.load().unwrap().expect("loads");
        assert_eq!(snapshot.downloads[0].added_by, None);

        snapshot.downloads[0].added_by = Some(AddedBy::Browser);
        store.save(&snapshot).unwrap();
        let back = store.load().unwrap().expect("loads");
        assert_eq!(back.downloads[0].added_by, Some(AddedBy::Browser));
    }
}
