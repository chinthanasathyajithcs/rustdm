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
    #[error("{}", quarantined_message(file, cfg!(windows)))]
    Quarantined { file: String },
    /// The program says it is not the version it was published as.
    #[error("{tool} {expected} reports itself as {reported:?}, so it was not installed.")]
    VersionMismatch {
        tool: Tool,
        expected: String,
        reported: String,
    },
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

fn quarantined_message(file: &str, windows: bool) -> String {
    if windows {
        format!("{file} was removed or blocked right after it was saved; Windows Security may have quarantined it (see Protection history).")
    } else {
        format!("{file} was removed or blocked right after it was saved.")
    }
}

/// An exit code as the system shows it: Windows reports 32-bit unsigned
/// values (3221225781, not -1073741515).
fn exit_code_text(code: i32, windows: bool) -> String {
    if windows {
        (code as u32).to_string()
    } else {
        code.to_string()
    }
}

fn run_check_message(tool: Tool, version: &str, code: Option<i32>, kept: Option<&str>) -> String {
    let outcome = match code {
        Some(code) => format!(
            "didn't start (exit {})",
            exit_code_text(code, cfg!(windows))
        ),
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
    /// Not enough room, from what the disk check reported. `target` is the path
    /// that was checked and `required` includes the safety margin; the message
    /// names the drive or mount point and the space needed without the margin.
    pub fn disk_full(tool: Tool, target: &str, required: u64, available: u64) -> Self {
        ToolError::DiskFull {
            tool,
            target: crate::disk::volume_label(std::path::Path::new(target)),
            needed: required.saturating_sub(crate::disk::SAFETY_MARGIN_BYTES),
            available,
        }
    }

    /// `file` was removed or blocked right after it was saved.
    pub fn quarantined(file: &std::path::Path) -> Self {
        ToolError::Quarantined {
            file: file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string()),
        }
    }

    /// What went wrong underneath, for the log and for the line shown after the
    /// message in `rdm tools`; the message itself stays short.
    pub fn detail(&self) -> Option<&str> {
        match self {
            ToolError::Network { detail, .. } | ToolError::BadSignature { detail } => Some(detail),
            _ => None,
        }
    }

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
        assert_eq!(exit_code_text(-1073741515, true), "3221225781");
        assert_eq!(exit_code_text(3, true), "3");
        assert_eq!(exit_code_text(-1, false), "-1");
        assert_eq!(exit_code_text(3, false), "3");
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
    fn the_quarantine_hint_is_only_for_windows() {
        assert!(quarantined_message("yt-dlp.exe", true).contains("Windows Security"));
        let elsewhere = quarantined_message("yt-dlp", false);
        assert_eq!(
            elsewhere,
            "yt-dlp was removed or blocked right after it was saved."
        );
    }

    #[test]
    fn the_disk_message_names_the_drive_and_leaves_out_the_margin() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tools/.staging/ffmpeg-8.1.3");
        let required = 210_000_000 + crate::disk::SAFETY_MARGIN_BYTES;
        let full = ToolError::disk_full(
            Tool::Ffmpeg,
            &target.display().to_string(),
            required,
            80_000_000,
        );
        let text = full.to_string();
        assert!(text.starts_with("Not enough space on "), "{text}");
        assert!(
            text.ends_with(" for ffmpeg (needs 210 MB, 80 MB free)."),
            "{text}"
        );
        assert!(!text.contains("staging"), "{text}");
    }

    #[test]
    fn network_and_signature_errors_keep_their_detail_out_of_the_message() {
        let net = ToolError::network(Tool::Deno, "connection reset");
        assert_eq!(net.detail(), Some("connection reset"));
        assert!(!net.to_string().contains("reset"));
        let bad = ToolError::BadSignature {
            detail: "the signature was not made by the pinned key".into(),
        };
        assert!(bad.detail().unwrap().contains("pinned key"));
        assert_eq!(ToolError::Cancelled.detail(), None);
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
