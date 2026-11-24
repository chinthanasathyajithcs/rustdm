use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("download cancelled")]
    Cancelled,
    #[error("{}", link_status_message(*.status))]
    LinkExpired { status: u16 },
    #[error("the link opened a web page, not a file")]
    WebPage,
    /// The link is an HLS playlist: it is read as a video, not saved as a file.
    #[error("the link is a video playlist")]
    Playlist,
    #[error("server file changed between sessions")]
    FileChanged,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("state error: {0}")]
    State(String),
    /// A helper tool (yt-dlp, ffmpeg, deno) could not be fetched or installed.
    /// `message` is shown to the user as it is; `transient` marks trouble that
    /// may go away by itself.
    #[error("{message}")]
    Tool { message: String, transient: bool },
    /// A video page could not be read, or has no video rustdm can download.
    /// `message` is shown to the user as it is; `transient` marks trouble that
    /// may go away by itself.
    #[error("{message}")]
    Video { message: String, transient: bool },
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

/// What to tell the user when the site refused a download's browser cookies.
pub fn sign_in_expired_message(status: u16) -> String {
    format!("sign-in expired (HTTP {status}); {START_AGAIN_FROM_BROWSER}")
}

/// What to tell the user when a download with cookies was answered with a web
/// page. It may be a sign-in page, or the link may just lead to a page: browser
/// cookies ride along on almost every caught download, so neither is assumed.
pub fn web_page_with_cookies_message() -> String {
    "the link opened a web page, not a file; \
     if the site wants you to sign in, sign in and start the download again from the browser"
        .to_string()
}

/// What to tell the user when the saved cookies ran out while the daemon was off.
pub fn sign_in_lapsed_message() -> String {
    format!("sign-in expired (the saved cookies have run out); {START_AGAIN_FROM_BROWSER}")
}

/// What to tell the user when a restart could not bring back a download's cookies.
pub fn saved_sign_in_unreadable_message() -> String {
    format!("saved sign-in could not be read; {START_AGAIN_FROM_BROWSER}")
}

const START_AGAIN_FROM_BROWSER: &str = "start the download again from the browser to continue it";

/// Statuses that mean the site did not accept the sign-in a download brought.
pub fn is_sign_in_status(status: u16) -> bool {
    matches!(status, 401 | 403)
}

impl Error {
    /// Network or server trouble that may go away by itself, so the download is
    /// worth trying again later. Disk, permission, and changed-file problems are not.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(_) | Error::Protocol(_) => true,
            Error::Tool { transient, .. } | Error::Video { transient, .. } => *transient,
            _ => false,
        }
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
        assert!(Error::Video {
            message: "slow".into(),
            transient: true
        }
        .is_transient());
        assert!(!Error::Video {
            message: "gone".into(),
            transient: false
        }
        .is_transient());
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
    fn a_video_error_shows_its_message_as_it_is() {
        let error = Error::Video {
            message: "This is a live stream.".into(),
            transient: false,
        };
        assert_eq!(error.to_string(), "This is a live stream.");
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
