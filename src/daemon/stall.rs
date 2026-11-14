//! Why an active download has stopped moving, in words for the TUI.

use crate::retry::{RetryCause, RetryNote};
use std::time::{Duration, Instant};

/// How long an active download may go without new data before it is shown as stalled.
/// Longer than the speed window, so short dips in a slow download don't count.
pub const STALL_NOTICE_AFTER: Duration = Duration::from_secs(5);

/// "no data for 42s: server busy (HTTP 503), retrying in 8s", or `None` while
/// data arrived within `STALL_NOTICE_AFTER`. A retry note from before the last
/// data is old news and left out.
pub fn describe(moved_at: Instant, retry: Option<RetryNote>, now: Instant) -> Option<String> {
    let quiet = now.saturating_duration_since(moved_at);
    if quiet < STALL_NOTICE_AFTER {
        return None;
    }
    let mut text = format!("no data for {}", format_quiet(quiet));
    if let Some(note) = retry.filter(|n| n.at >= moved_at) {
        text.push_str(": ");
        text.push_str(cause_text(note.cause).as_str());
        text.push_str(", ");
        text.push_str(&next_attempt_text(note.next_at, now));
    }
    Some(text)
}

fn cause_text(cause: RetryCause) -> String {
    match cause {
        RetryCause::Unreachable => "can't reach the server".to_string(),
        RetryCause::NoAnswer => "server not answering".to_string(),
        RetryCause::Busy { status } => format!("server busy (HTTP {status})"),
        RetryCause::Unexpected { status } => format!("unexpected answer (HTTP {status})"),
        RetryCause::WrongPart => "server sent the wrong part of the file".to_string(),
        RetryCause::Dropped => "connection dropped".to_string(),
    }
}

/// "retrying in 8s" while the wait runs, "reconnecting" once the attempt is under way.
fn next_attempt_text(next_at: Instant, now: Instant) -> String {
    let left = next_at.saturating_duration_since(now);
    if left.is_zero() {
        return "reconnecting".to_string();
    }
    // Round up, so the count never shows 0s while there is still a wait.
    let secs = left.as_millis().div_ceil(1000);
    format!("retrying in {secs}s")
}

/// "42s", "3m 05s", or "1h 02m".
fn format_quiet(quiet: Duration) -> String {
    let s = quiet.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn note(cause: RetryCause, at: Instant, wait: Duration) -> Option<RetryNote> {
        Some(RetryNote {
            cause,
            at,
            next_at: at + wait,
        })
    }

    #[test]
    fn a_short_gap_in_the_data_is_not_a_stall() {
        let moved = Instant::now();
        assert_eq!(describe(moved, None, moved + secs(4)), None);
    }

    #[test]
    fn a_long_gap_says_how_long_it_has_been() {
        let moved = Instant::now();
        assert_eq!(
            describe(moved, None, moved + secs(42)).as_deref(),
            Some("no data for 42s")
        );
        assert_eq!(
            describe(moved, None, moved + secs(185)).as_deref(),
            Some("no data for 3m 05s")
        );
        assert_eq!(
            describe(moved, None, moved + secs(3720)).as_deref(),
            Some("no data for 1h 02m")
        );
    }

    #[test]
    fn a_retry_during_the_gap_gives_the_reason_and_the_wait() {
        let moved = Instant::now();
        let busy = note(RetryCause::Busy { status: 503 }, moved + secs(30), secs(10));
        assert_eq!(
            describe(moved, busy, moved + secs(32)).as_deref(),
            Some("no data for 32s: server busy (HTTP 503), retrying in 8s")
        );
    }

    #[test]
    fn once_the_wait_is_over_it_says_reconnecting() {
        let moved = Instant::now();
        let dropped = note(RetryCause::Dropped, moved + secs(30), secs(2));
        assert_eq!(
            describe(moved, dropped, moved + secs(33)).as_deref(),
            Some("no data for 33s: connection dropped, reconnecting")
        );
    }

    #[test]
    fn a_wait_under_a_second_still_counts_as_one() {
        let moved = Instant::now();
        let at = moved + secs(10);
        let silent = note(RetryCause::NoAnswer, at, Duration::from_millis(1500));
        assert_eq!(
            describe(moved, silent, at + Duration::from_millis(900)).as_deref(),
            Some("no data for 10s: server not answering, retrying in 1s")
        );
    }

    #[test]
    fn a_retry_from_before_the_last_data_is_left_out() {
        let start = Instant::now();
        let old = note(RetryCause::Unreachable, start, secs(1));
        let moved = start + secs(5);
        assert_eq!(
            describe(moved, old, moved + secs(20)).as_deref(),
            Some("no data for 20s")
        );
    }

    #[test]
    fn every_cause_reads_as_plain_words() {
        assert_eq!(
            cause_text(RetryCause::Unreachable),
            "can't reach the server"
        );
        assert_eq!(
            cause_text(RetryCause::Unexpected { status: 500 }),
            "unexpected answer (HTTP 500)"
        );
        assert_eq!(
            cause_text(RetryCause::WrongPart),
            "server sent the wrong part of the file"
        );
    }
}
