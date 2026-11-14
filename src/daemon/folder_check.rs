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
    probe_write_with(folder, |probe| std::fs::remove_file(probe))
}

/// `probe_write`, removing the probe file with `remove`. Creating the file is
/// the test; a file that can't be removed straight away (an antivirus or sync
/// client holding it on Windows) leaves the folder writable, so that is only
/// logged.
fn probe_write_with(folder: &Path, remove: impl Fn(&Path) -> io::Result<()>) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let probe = folder.join(format!(".rdm-write-test-{}-{n}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    if let Err(e) = remove(&probe) {
        tracing::warn!(
            "Could not remove the write test file {}: {e}",
            probe.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_file_that_cannot_be_removed_still_counts_as_writable() {
        let dir = tempfile::tempdir().unwrap();
        let held = |_: &Path| Err(io::Error::new(io::ErrorKind::PermissionDenied, "in use"));
        assert!(probe_write_with(dir.path(), held).is_ok());
    }

    #[test]
    fn a_folder_that_cannot_take_a_new_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        // Creating inside a folder that isn't there fails like a refused write.
        assert!(probe_write_with(&missing, |p| std::fs::remove_file(p)).is_err());
    }
}
