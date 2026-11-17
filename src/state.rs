use crate::error::Error;
use crate::probe::Validator;
use crate::ranges::ByteRange;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const STATE_VERSION: u32 = 1;
pub const CHECKPOINT_BYTES: u64 = 4 * 1024 * 1024;
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub total: u64,
    pub validator: Validator,
    pub completed: Vec<ByteRange>,
    pub output: PathBuf,
    /// Connections the download was started with, so a resume uses the same
    /// number. Missing from state files written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
}

pub fn state_path(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part.rdm", output.display()))
}

pub fn part_path(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", output.display()))
}

pub fn save_atomic(path: &Path, state: &State) -> Result<(), Error> {
    let json =
        serde_json::to_vec_pretty(state).map_err(|e| Error::State(format!("json error: {e}")))?;
    write_atomic(path, &json)?;
    Ok(())
}

/// Write `bytes` to a temp file next to `path`, flush it to disk, then rename it
/// over `path`, so readers see either the old or the new contents.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)?;
    finish_atomic(file, &tmp_path, path, bytes)
}

/// `write_atomic` for secrets: on Unix the temp file is created readable by the
/// owner alone, so the contents are never visible to others, not even briefly.
/// On Windows the file inherits its folder's access rules. The caller names the
/// temp file, so that two writers never share one.
pub fn write_atomic_private(path: &Path, tmp_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // A temp file left by a crash keeps whatever permissions it was made with.
    match std::fs::remove_file(tmp_path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(tmp_path)?;
    let written = finish_atomic(file, tmp_path, path, bytes);
    if written.is_err() {
        // Do not leave half a secret behind.
        let _ = std::fs::remove_file(tmp_path);
    }
    written
}

fn finish_atomic(
    mut file: std::fs::File,
    tmp_path: &Path,
    path: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(tmp_path, path)
}

pub fn load(path: &Path) -> Result<State, Error> {
    let bytes = std::fs::read(path)?;
    let state: State = serde_json::from_slice(&bytes)
        .map_err(|e| Error::State(format!("failed to parse state file: {e}")))?;

    if state.version != STATE_VERSION {
        return Err(Error::State(format!(
            "unknown state version {}, expected {}",
            state.version, STATE_VERSION
        )));
    }

    Ok(state)
}

pub fn should_checkpoint(bytes_since_last: u64, since_last: Duration) -> bool {
    bytes_since_last >= CHECKPOINT_BYTES || since_last >= CHECKPOINT_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_state(output: PathBuf) -> State {
        State {
            version: STATE_VERSION,
            url: "https://example.com/file.bin".to_string(),
            final_url: "https://example.com/file.bin".to_string(),
            total: 10 * 1024 * 1024,
            validator: Validator {
                etag: Some("\"v1\"".to_string()),
                last_modified: None,
            },
            completed: vec![ByteRange {
                start: 0,
                end: 5 * 1024 * 1024,
            }],
            output,
            connections: Some(4),
        }
    }

    #[test]
    fn older_state_files_without_a_connection_count_still_load() {
        let mut json = serde_json::to_value(sample_state(PathBuf::from("/tmp/f.bin"))).unwrap();
        json.as_object_mut().unwrap().remove("connections");
        let state: State = serde_json::from_value(json).unwrap();
        assert_eq!(state.connections, None);
    }

    #[test]
    fn state_round_trips_through_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.part.rdm");
        let state = sample_state(dir.path().join("file.bin"));

        save_atomic(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn save_atomic_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.part.rdm");
        let state = sample_state(dir.path().join("file.bin"));

        save_atomic(&path, &state).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("tmp").exists());
        let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
        assert!(!tmp_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_readable_by_the_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.bin");
        // A leftover temp file with loose permissions must not be reused.
        let leftover = dir.path().join("secret.bin.tmp");
        std::fs::write(&leftover, b"old").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic_private(&path, &leftover, b"hush").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hush");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!leftover.exists());
    }

    #[test]
    fn a_private_write_replaces_the_old_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("secret.bin");
        write_atomic_private(&path, &dir.path().join("secret.bin.tmp"), b"first").unwrap();
        write_atomic_private(&path, &dir.path().join("secret.bin.tmp"), b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn corrupt_state_file_is_reported_not_ignored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.part.rdm");
        std::fs::write(&path, b"{not json").unwrap();

        let err = load(&path).unwrap_err();
        match err {
            Error::State(_) => {}
            other => panic!("expected Error::State, got {other:?}"),
        }
    }

    #[test]
    fn truncated_state_file_is_reported() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.part.rdm");
        let state = sample_state(dir.path().join("file.bin"));

        save_atomic(&path, &state).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let truncated = &bytes[..40.min(bytes.len())];
        std::fs::write(&path, truncated).unwrap();

        let err = load(&path).unwrap_err();
        match err {
            Error::State(_) => {}
            other => panic!("expected Error::State, got {other:?}"),
        }
    }

    #[test]
    fn unknown_state_version_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.part.rdm");
        let mut state = sample_state(dir.path().join("file.bin"));
        state.version = 99;

        let json = serde_json::to_string(&state).unwrap();
        std::fs::write(&path, json).unwrap();

        let err = load(&path).unwrap_err();
        match err {
            Error::State(msg) => assert!(msg.contains("version")),
            other => panic!("expected Error::State with version msg, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_fires_on_bytes_or_time() {
        assert!(should_checkpoint(CHECKPOINT_BYTES, Duration::ZERO));
        assert!(should_checkpoint(0, CHECKPOINT_INTERVAL));
        assert!(!should_checkpoint(1, Duration::from_secs(1)));
    }
}
