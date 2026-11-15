use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("download cancelled")]
    Cancelled,
    #[error("{}", link_status_message(*.status))]
    LinkExpired { status: u16 },
    #[error("the link opened a web page, not a file")]
    WebPage,
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

/// Statuses that mean this link will not work as it is: stop at once, no retry.
pub fn is_link_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 410)
}

/// What to tell the user when `status` stopped a download.
pub fn link_status_message(status: u16) -> String {
    if status == 401 {
        format!("link needs sign-in (HTTP {status})")
    } else {
        format!("link expired or removed (HTTP {status})")
    }
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
        assert!(!Error::WebPage.is_transient());
        let disk_full = std::io::Error::new(std::io::ErrorKind::StorageFull, "full");
        assert!(!Error::Io(disk_full).is_transient());
        assert!(!Error::InsufficientDiskSpace {
            target: "C:".into(),
            required: 2,
            available: 1
        }
        .is_transient());
    }

    #[test]
    fn a_401_says_the_link_needs_sign_in() {
        assert_eq!(link_status_message(401), "link needs sign-in (HTTP 401)");
        assert_eq!(
            link_status_message(403),
            "link expired or removed (HTTP 403)"
        );
        assert_eq!(
            Error::LinkExpired { status: 401 }.to_string(),
            "link needs sign-in (HTTP 401)"
        );
    }

    #[test]
    fn only_these_statuses_stop_a_download_at_once() {
        for status in [401, 403, 404, 410] {
            assert!(is_link_status(status));
        }
        for status in [200, 400, 429, 500, 503] {
            assert!(!is_link_status(status));
        }
    }
}
