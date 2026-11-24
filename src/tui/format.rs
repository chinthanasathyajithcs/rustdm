use crate::ipc::protocol::{DownloadStatus, DownloadSummary};
use crate::tui::theme;
use ratatui::text::Span;

pub fn status_label(status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Queued => "queued",
        DownloadStatus::Active => "active",
        DownloadStatus::Pausing => "pausing",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Retrying => "retrying",
        DownloadStatus::Completed => "completed",
        DownloadStatus::Failed => "failed",
        DownloadStatus::Cancelled => "cancelled",
        DownloadStatus::Choosing => "choose",
    }
}

/// The status word for a download, in its colour: "stalled" for an active
/// download that has had no data for a while.
pub fn status_span(status: DownloadStatus, stalled: bool) -> Span<'static> {
    if stalled && status == DownloadStatus::Active {
        Span::styled("stalled", theme::STALLED)
    } else {
        Span::styled(status_label(status), theme::status_style(status))
    }
}

/// What to say about a download's current step: the daemon's own text, or,
/// for a video waiting for a quality, what the user can do about it.
pub fn step_text(d: &DownloadSummary) -> Option<String> {
    match (&d.step, d.status) {
        (Some(step), _) => Some(step.clone()),
        (None, DownloadStatus::Choosing) => Some("Choose a quality (Enter)".to_string()),
        (None, _) => None,
    }
}

/// Binary units: 1536 bytes is "1.5 KiB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// "2.0 MiB/s", or "-" when nothing is moving.
pub fn format_speed(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        "-".to_string()
    } else {
        format!("{}/s", format_bytes(bytes_per_sec))
    }
}

/// "512.0 KiB/s", or "unlimited" for no cap.
pub fn format_speed_cap(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        "unlimited".to_string()
    } else {
        format!("{}/s", format_bytes(bytes_per_sec))
    }
}

/// "45s", "3m 20s", "2h 05m", or "-" when unknown.
pub fn format_eta(secs: Option<u64>) -> String {
    match secs {
        None => "-".to_string(),
        Some(s) if s >= 3600 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        Some(s) if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        Some(s) => format!("{s}s"),
    }
}

/// Share of the file on disk, from 0.0 to 1.0, once the size is known.
pub fn fraction_done(d: &DownloadSummary) -> Option<f64> {
    if d.status == DownloadStatus::Completed {
        return Some(1.0);
    }
    match d.total_bytes {
        Some(total) if total > 0 => Some((d.downloaded_bytes as f64 / total as f64).min(1.0)),
        _ => None,
    }
}

/// One plain-text line per download, used by `rdm list` and the plain dashboard.
pub fn format_row(d: &DownloadSummary) -> String {
    let about = if d.video.as_ref().is_some_and(|v| v.size_is_estimate) {
        "~"
    } else {
        ""
    };
    let progress = match (d.total_bytes, fraction_done(d)) {
        (Some(total), Some(fraction)) if total > 0 => format!(
            "{} / {about}{} ({:.0}%)",
            format_bytes(d.downloaded_bytes),
            format_bytes(total),
            fraction * 100.0
        ),
        _ => format_bytes(d.downloaded_bytes),
    };

    let mut row = format!(
        "{:<8} {:<10} {:<28} {:>12}  {}",
        d.id,
        status_label(d.status),
        progress,
        format_speed(d.speed_bps),
        d.filename
    );
    if d.max_bytes_per_sec > 0 {
        row.push_str(&format!(
            "  [cap {}]",
            format_speed_cap(d.max_bytes_per_sec)
        ));
    }
    if let Some(err) = &d.error_message {
        row.push_str(&format!("  ({err})"));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(64 * 1024 * 1024), "64.0 MiB");
    }

    #[test]
    fn choosing_rows_say_choose() {
        assert_eq!(status_label(DownloadStatus::Choosing), "choose");
        let summary = crate::tui::state::test_support::summary("dl_1", DownloadStatus::Choosing);
        let row = format_row(&summary);
        assert!(row.contains("choose "), "{row}");
    }

    #[test]
    fn eta_drops_seconds_once_hours_appear() {
        assert_eq!(format_eta(None), "-");
        assert_eq!(format_eta(Some(45)), "45s");
        assert_eq!(format_eta(Some(200)), "3m 20s");
        assert_eq!(format_eta(Some(7500)), "2h 05m");
    }

    #[test]
    fn list_rows_show_percent_once_the_size_is_known() {
        let summary = DownloadSummary {
            id: "dl_3".to_string(),
            url: "http://example.com/a.iso".to_string(),
            destination: PathBuf::from("/downloads"),
            filename: "a.iso".to_string(),
            status: DownloadStatus::Active,
            downloaded_bytes: 25 * 1024 * 1024,
            total_bytes: Some(100 * 1024 * 1024),
            speed_bps: 2 * 1024 * 1024,
            eta_secs: Some(37),
            error_message: None,
            max_bytes_per_sec: 0,
            video: None,
            step: None,
        };
        let row = format_row(&summary);
        assert!(row.contains("25.0 MiB / 100.0 MiB (25%)"), "{row}");
        assert!(row.contains("2.0 MiB/s"), "{row}");
        assert!(row.ends_with("a.iso"), "{row}");

        let capped = DownloadSummary {
            max_bytes_per_sec: 512 * 1024,
            ..summary
        };
        assert!(format_row(&capped).ends_with("a.iso  [cap 512.0 KiB/s]"));
    }

    #[test]
    fn an_estimated_size_has_a_tilde() {
        let mut summary = crate::tui::state::test_support::summary("dl_4", DownloadStatus::Active);
        summary.downloaded_bytes = 25 * 1024 * 1024;
        summary.total_bytes = Some(100 * 1024 * 1024);
        summary.video = Some(crate::ipc::protocol::VideoSummary {
            title: "A video".to_string(),
            duration_secs: None,
            choices: Vec::new(),
            default: None,
            chosen: None,
            size_is_estimate: true,
        });

        let row = format_row(&summary);

        assert!(row.contains("25.0 MiB / ~100.0 MiB (25%)"), "{row}");
    }
}
