//! What the TUI asks for: a daemon call, or quitting.

use crate::ipc::protocol::QueuePosition;

/// Something the dashboard asks the daemon to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Add(String),
    Pause(String),
    Resume(String),
    Cancel(String),
    Remove(String),
    /// Move a download one place up or down the queue.
    Move(String, QueuePosition),
    Quit,
}
