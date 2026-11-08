use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("download cancelled")]
    Cancelled,
    #[error("link expired or removed (status {status})")]
    LinkExpired { status: u16 },
    #[error("server file changed between sessions")]
    FileChanged,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("state error: {0}")]
    State(String),
    #[error("insufficient disk space on {target}: required {required} bytes, available {available} bytes")]
    InsufficientDiskSpace {
        target: String,
        required: u64,
        available: u64,
    },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Network or server trouble that may go away by itself, so the download is
    /// worth trying again later. Disk, permission, and changed-file problems are not.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Http(_) | Error::Protocol(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_network_and_server_errors_are_transient() {
        assert!(Error::Protocol("status 500".into()).is_transient());
        assert!(!Error::FileChanged.is_transient());
        assert!(!Error::LinkExpired { status: 404 }.is_transient());
        assert!(!Error::Cancelled.is_transient());
        let disk_full = std::io::Error::new(std::io::ErrorKind::StorageFull, "full");
        assert!(!Error::Io(disk_full).is_transient());
        assert!(!Error::InsufficientDiskSpace {
            target: "C:".into(),
            required: 2,
            available: 1
        }
        .is_transient());
    }
}
