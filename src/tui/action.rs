//! What the TUI asks for: a daemon call, or quitting.

use crate::ipc::protocol::{AddDownloadParams, QueuePosition};

/// Something the dashboard asks the daemon to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Add each of these downloads, one `download.add` per link.
    Add(Vec<AddDownloadParams>),
    Pause(String),
    Resume(String),
    /// Stop a download for good; `delete_files` also removes its partial files.
    Cancel {
        id: String,
        delete_files: bool,
    },
    /// Cap one download's speed; 0 removes the cap.
    SetSpeed {
        id: String,
        max_bytes_per_sec: u64,
    },
    /// Stop rdmd, which pauses active downloads until it starts again.
    StopDaemon,
    Remove(String),
    /// Move a download one place up or down the queue.
    Move(String, QueuePosition),
    Quit,
}
