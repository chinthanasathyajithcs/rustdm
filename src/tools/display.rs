//! Words for a tool's state, shared by `rdm tools` and the Settings screen.

use crate::ipc::protocol::{ToolInfo, ToolName, ToolPhase};

/// Sizes are shown in megabytes (1,000,000 bytes), as in the other messages.
const MB: f64 = 1_000_000.0;

/// "yt-dlp", "ffmpeg", "deno".
pub fn tool_label(tool: ToolName) -> &'static str {
    match tool {
        ToolName::YtDlp => "yt-dlp",
        ToolName::Ffmpeg => "ffmpeg",
        ToolName::Deno => "deno",
    }
}

/// How long ago a check was, as "never", "just now", "5 min ago", "3 h ago", "2 d ago".
pub fn age_text(last_checked_unix: Option<u64>, now_unix: u64) -> String {
    let Some(at) = last_checked_unix else {
        return "never".to_string();
    };
    let secs = now_unix.saturating_sub(at);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// What is happening with the tool: progress while it is being fetched, the
/// reason for the last failure, or whether it is installed.
pub fn state_text(info: &ToolInfo) -> String {
    let name = tool_label(info.tool);
    match info.phase {
        ToolPhase::Checking => format!("Checking for a newer {name}"),
        ToolPhase::Downloading => match info.total_bytes {
            Some(total) => format!(
                "Getting {name}: {:.0}/{:.0} MB",
                info.downloaded_bytes as f64 / MB,
                total as f64 / MB
            ),
            None => format!(
                "Getting {name}: {:.0} MB",
                info.downloaded_bytes as f64 / MB
            ),
        },
        ToolPhase::Verifying => format!("Verifying {name}"),
        ToolPhase::Installing => format!("Installing {name}"),
        ToolPhase::Idle => match (&info.error, &info.installed) {
            (Some(error), _) => match &info.detail {
                Some(detail) => format!("{error} ({detail})"),
                None => error.clone(),
            },
            (None, Some(_)) => "installed".to_string(),
            (None, None) => "not installed; downloaded when first needed".to_string(),
        },
    }
}

/// One line per tool under a header, aligned in columns.
pub fn table(infos: &[ToolInfo], now_unix: u64) -> String {
    let dash = || "-".to_string();
    let mut rows = vec![[
        "TOOL".to_string(),
        "VERSION".to_string(),
        "PREVIOUS".to_string(),
        "LAST CHECK".to_string(),
        "STATE".to_string(),
    ]];
    for info in infos {
        rows.push([
            tool_label(info.tool).to_string(),
            info.installed.clone().unwrap_or_else(dash),
            info.previous.clone().unwrap_or_else(dash),
            age_text(info.last_checked_unix, now_unix),
            state_text(info),
        ]);
    }
    let widths: Vec<usize> = (0..4)
        .map(|col| {
            rows.iter()
                .map(|row| row[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (col, width) in widths.iter().enumerate() {
                line.push_str(&format!("{:<width$}  ", row[col], width = width));
            }
            line.push_str(&row[4]);
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(tool: ToolName) -> ToolInfo {
        ToolInfo {
            tool,
            installed: None,
            previous: None,
            last_checked_unix: None,
            phase: ToolPhase::Idle,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
            detail: None,
        }
    }

    #[test]
    fn ages_read_naturally() {
        let now = 1_000_000;
        assert_eq!(age_text(None, now), "never");
        assert_eq!(age_text(Some(now - 5), now), "just now");
        assert_eq!(age_text(Some(now - 300), now), "5 min ago");
        assert_eq!(age_text(Some(now - 3 * 3600), now), "3 h ago");
        assert_eq!(age_text(Some(now - 2 * 86_400), now), "2 d ago");
        assert_eq!(age_text(Some(now + 10), now), "just now");
    }

    #[test]
    fn state_shows_progress_errors_and_installation() {
        let mut tool = info(ToolName::YtDlp);
        assert!(state_text(&tool).starts_with("not installed"));
        tool.installed = Some("2026.11.14".into());
        assert_eq!(state_text(&tool), "installed");
        tool.error = Some("Couldn't reach github.com to get yt-dlp; will retry.".into());
        assert_eq!(
            state_text(&tool),
            "Couldn't reach github.com to get yt-dlp; will retry."
        );
        tool.detail = Some("dns error".into());
        assert_eq!(
            state_text(&tool),
            "Couldn't reach github.com to get yt-dlp; will retry. (dns error)"
        );
        tool.detail = None;

        tool.phase = ToolPhase::Downloading;
        tool.downloaded_bytes = 12_000_000;
        tool.total_bytes = Some(24_000_000);
        assert_eq!(state_text(&tool), "Getting yt-dlp: 12/24 MB");
        tool.total_bytes = None;
        assert_eq!(state_text(&tool), "Getting yt-dlp: 12 MB");
        tool.phase = ToolPhase::Verifying;
        assert_eq!(state_text(&tool), "Verifying yt-dlp");
    }

    #[test]
    fn the_table_aligns_its_columns() {
        let mut ytdlp = info(ToolName::YtDlp);
        ytdlp.installed = Some("2026.11.14.200000".into());
        ytdlp.previous = Some("2026.11.10.100000".into());
        ytdlp.last_checked_unix = Some(100);
        let text = table(&[ytdlp, info(ToolName::Deno)], 100 + 7200);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("TOOL"));
        assert!(lines[1].contains("2026.11.14.200000"));
        assert!(lines[1].contains("2 h ago"));
        // The STATE column starts at the same place on every line.
        let state_col = lines[0].find("STATE").unwrap();
        assert_eq!(lines[1].find("installed"), Some(state_col));
        assert_eq!(lines[2].find("not installed"), Some(state_col));
    }
}
