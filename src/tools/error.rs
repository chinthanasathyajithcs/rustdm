//! Everything that can go wrong while fetching or installing a tool, with the
//! messages the user sees (spec section 2.8).

use super::Tool;
use crate::error::Error;
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// GitHub could not be reached, or answered something unusable.
    #[error("Couldn't reach github.com to get {tool}; will retry.")]
    Network { tool: Tool, detail: String },
    /// GitHub is limiting requests until `until_unix`.
    #[error("{}", rate_limited_message(*tool, *until_unix, installed.as_deref()))]
    RateLimited {
        tool: Tool,
        until_unix: Option<u64>,
        installed: Option<String>,
    },
    #[error(
        "{tool} {version} didn't match its published checksum, so it was deleted and not used."
    )]
    ChecksumMismatch { tool: Tool, version: String },
    #[error("The yt-dlp release list isn't signed by yt-dlp's key; nothing was installed.")]
    BadSignature { detail: String },
    #[error("Not enough space on {target} for {tool} (needs {}, {} free).", mb(*needed), mb(*available))]
    DiskFull {
        tool: Tool,
        target: String,
        needed: u64,
        available: u64,
    },
    #[error("{file} was removed or blocked right after it was saved; Windows Security may have quarantined it (see Protection history).")]
    Quarantined { file: String },
    #[error("{}", run_check_message(*tool, version, *code, kept.as_deref()))]
    RunCheckFailed {
        tool: Tool,
        version: String,
        code: Option<i32>,
        kept: Option<String>,
    },
    #[error("tool download cancelled")]
    Cancelled,
    /// The release did not have what we need (no asset for this platform, a
    /// malformed answer, an archive without the program in it).
    #[error("{0}")]
    Unusable(String),
    #[error("{tool} could not be installed: {source}")]
    Io {
        tool: Tool,
        #[source]
        source: io::Error,
    },
}

fn mb(bytes: u64) -> String {
    format!("{} MB", bytes.div_ceil(1_000_000))
}

fn rate_limited_message(tool: Tool, until_unix: Option<u64>, installed: Option<&str>) -> String {
    let when = match until_unix {
        Some(unix) => format!("until {}", clock_utc(unix)),
        None => "for a while".to_string(),
    };
    match installed {
        Some(version) => format!("GitHub is limiting update checks {when}; using {tool} {version}"),
        None => format!("GitHub is limiting update checks {when}; can't get {tool} until then"),
    }
}

fn run_check_message(tool: Tool, version: &str, code: Option<i32>, kept: Option<&str>) -> String {
    let outcome = match code {
        Some(code) => format!("didn't start (exit {code})"),
        None => "didn't start".to_string(),
    };
    match kept {
        Some(old) => format!("{tool} {version} {outcome}; still using {old}."),
        None => format!("{tool} {version} {outcome}."),
    }
}

/// `HH:MM UTC` for a unix time. No time-zone database is linked in, so the
/// clock is shown in UTC rather than local time.
fn clock_utc(unix: u64) -> String {
    let secs_today = unix % 86_400;
    format!("{:02}:{:02} UTC", secs_today / 3600, secs_today % 3600 / 60)
}

impl ToolError {
    /// Trouble that may go away by itself, so a task should try again later.
    pub fn is_transient(&self) -> bool {
        matches!(self, ToolError::Network { .. })
    }

    /// A transport failure from `reqwest`, `tool` being the one we were getting.
    pub fn network(tool: Tool, detail: impl std::fmt::Display) -> Self {
        ToolError::Network {
            tool,
            detail: detail.to_string(),
        }
    }
}

impl From<ToolError> for Error {
    fn from(err: ToolError) -> Self {
        match err {
            ToolError::Cancelled => Error::Cancelled,
            ToolError::Io { source, .. } if source.kind() == io::ErrorKind::StorageFull => {
                Error::Io(source)
            }
            other => Error::Tool {
                transient: other.is_transient(),
                message: other.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_the_spec_wording() {
        let offline = ToolError::network(Tool::YtDlp, "dns");
        assert_eq!(
            offline.to_string(),
            "Couldn't reach github.com to get yt-dlp; will retry."
        );

        // 2026-11-16 14:32:00 UTC
        let at = 1_794_839_520;
        let limited = ToolError::RateLimited {
            tool: Tool::YtDlp,
            until_unix: Some(at),
            installed: Some("2026.11.10".into()),
        };
        assert_eq!(
            limited.to_string(),
            "GitHub is limiting update checks until 14:32 UTC; using yt-dlp 2026.11.10"
        );
        let nothing = ToolError::RateLimited {
            tool: Tool::YtDlp,
            until_unix: Some(at),
            installed: None,
        };
        assert_eq!(
            nothing.to_string(),
            "GitHub is limiting update checks until 14:32 UTC; can't get yt-dlp until then"
        );

        let mismatch = ToolError::ChecksumMismatch {
            tool: Tool::YtDlp,
            version: "2026.11.14".into(),
        };
        assert_eq!(
            mismatch.to_string(),
            "yt-dlp 2026.11.14 didn't match its published checksum, so it was deleted and not used."
        );

        let failed = ToolError::RunCheckFailed {
            tool: Tool::Ffmpeg,
            version: "8.1.3".into(),
            code: Some(3221225781u32 as i32),
            kept: Some("8.1.2".into()),
        };
        assert!(failed.to_string().ends_with("still using 8.1.2."));
        assert!(failed.to_string().starts_with("ffmpeg 8.1.3 didn't start"));

        let full = ToolError::DiskFull {
            tool: Tool::Ffmpeg,
            target: "C:".into(),
            needed: 210_000_000,
            available: 80_000_000,
        };
        assert_eq!(
            full.to_string(),
            "Not enough space on C: for ffmpeg (needs 210 MB, 80 MB free)."
        );
    }

    #[test]
    fn only_a_network_error_makes_the_task_retry() {
        let transient: Error = ToolError::network(Tool::Deno, "reset").into();
        assert!(transient.is_transient());
        for stop in [
            ToolError::BadSignature { detail: "x".into() },
            ToolError::Unusable("no asset".into()),
            ToolError::Quarantined {
                file: "yt-dlp.exe".into(),
            },
        ] {
            let err: Error = stop.into();
            assert!(!err.is_transient(), "{err}");
        }
        assert!(matches!(
            Error::from(ToolError::Cancelled),
            Error::Cancelled
        ));
    }
}
