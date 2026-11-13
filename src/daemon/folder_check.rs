//! Checking that a download folder can be used before anything is queued in it.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a folder can't take downloads.
#[derive(Debug)]
pub enum FolderProblem {
    NotFullPath,
    NotAFolder,
    CantWrite(io::Error),
}

impl fmt::Display for FolderProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FolderProblem::NotFullPath => f.write_str("must be a full path"),
            FolderProblem::NotAFolder => f.write_str("is a file, not a folder"),
            FolderProblem::CantWrite(e) => write!(f, "can't write there ({e})"),
        }
    }
}

/// `folder` must be a full path. A missing folder is made when a download
/// starts, so the closest part of it that exists must be a folder the daemon
/// can write in.
pub fn check_folder(folder: &Path) -> Result<(), FolderProblem> {
    if !folder.is_absolute() {
        return Err(FolderProblem::NotFullPath);
    }
    let Some(existing) = folder.ancestors().find(|p| p.exists()) else {
        return Ok(()); // Nothing to check against, not even the root.
    };
    if !existing.is_dir() {
        return Err(FolderProblem::NotAFolder);
    }
    probe_write(existing).map_err(FolderProblem::CantWrite)
}

/// Create and remove an empty file in `folder`. Each probe has its own name,
/// so two checks running at once never remove each other's file.
fn probe_write(folder: &Path) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let probe = folder.join(format!(".rdm-write-test-{}-{n}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}
