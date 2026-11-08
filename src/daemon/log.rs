use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub const LOG_FILE_NAME: &str = "rdmd.log";
/// Size at which the log is moved aside to `rdmd.log.old` on the next start.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_FILE_NAME)
}

/// Open the daemon's log in `data_dir` for appending, creating the folder if
/// needed. A log over `MAX_LOG_BYTES` replaces `rdmd.log.old` first, so at
/// most two logs are kept.
pub fn open_log_file(data_dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    let path = log_path(data_dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let old = data_dir.join(format!("{LOG_FILE_NAME}.old"));
        std::fs::rename(&path, old)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_log_is_created_with_its_folder_and_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("new").join("data");
        writeln!(open_log_file(&data).unwrap(), "first").unwrap();
        writeln!(open_log_file(&data).unwrap(), "second").unwrap();
        let text = std::fs::read_to_string(log_path(&data)).unwrap();
        assert_eq!(text, "first\nsecond\n");
    }

    #[test]
    fn a_large_log_is_moved_aside_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; MAX_LOG_BYTES as usize + 1];
        std::fs::write(log_path(dir.path()), &big).unwrap();

        writeln!(open_log_file(dir.path()).unwrap(), "fresh").unwrap();
        assert_eq!(
            std::fs::read_to_string(log_path(dir.path())).unwrap(),
            "fresh\n"
        );
        let old = std::fs::metadata(dir.path().join("rdmd.log.old")).unwrap();
        assert_eq!(old.len(), big.len() as u64);
    }
}
