//! What the TUI asks for: a daemon call, or quitting.

use std::path::PathBuf;

use crate::ipc::protocol::{
    AddDownloadParams, QueuePosition, SetDefaultsParams, SetLimitsParams, ToolName,
};

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
    /// Change the daemon's limits; fields left out keep their value.
    SetLimits(SetLimitsParams),
    /// Change the defaults for new downloads; fields left out keep their value.
    SetDefaults(SetDefaultsParams),
    /// Check a helper tool for a newer version, installing it if it is missing.
    UpdateTool(ToolName),
    /// Go back to a helper tool's previous version.
    RollbackTool(ToolName),
    /// Stop rdmd, which pauses active downloads until it starts again.
    StopDaemon,
    Remove(String),
    /// Show this download's file, or the folder it goes in, in the file manager.
    OpenFolder(PathBuf),
    /// Move a download one place up or down the queue.
    Move(String, QueuePosition),
    Quit,
}
