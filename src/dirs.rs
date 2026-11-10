//! Well-known folders.

use std::path::PathBuf;

/// The user's Downloads folder: `%USERPROFILE%\Downloads` on Windows,
/// `$HOME/Downloads` elsewhere, or a relative `Downloads` when neither is set.
pub fn default_downloads_dir() -> PathBuf {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        PathBuf::from(userprofile).join("Downloads")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("Downloads")
    }
}
