use std::time::{Duration, Instant};

pub const MAX_SEGMENT_ATTEMPTS: u32 = 5;
pub const BACKOFF_BASE: Duration = Duration::from_millis(500);
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

pub fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let base_secs = BACKOFF_BASE.as_secs_f64() * 2f64.powi(attempt.min(30) as i32);
    let capped_secs = base_secs.min(BACKOFF_CAP.as_secs_f64());
    let factor = 0.75 + 0.5 * jitter;
    Duration::from_secs_f64(capped_secs * factor)
}

pub fn retry_after_secs(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Why a connection is about to try again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryCause {
    /// The connection to the server could not be opened.
    Unreachable,
    /// The server took the connection but did not answer in time.
    NoAnswer,
    /// The server said it is too busy (429 or 503).
    Busy { status: u16 },
    /// The server answered with a status the download cannot use.
    Unexpected { status: u16 },
    /// The server sent a different part of the file than was asked for.
    WrongPart,
    /// The connection closed or went silent part-way through the data.
    Dropped,
}

impl RetryCause {
    /// The cause of a request that failed before any answer arrived.
    pub fn of_failed_request(err: &reqwest::Error) -> Self {
        if err.is_connect() {
            Self::Unreachable
        } else if err.is_timeout() {
            Self::NoAnswer
        } else {
            Self::Dropped
        }
    }
}

/// The latest trouble any of a download's connections ran into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryNote {
    pub cause: RetryCause,
    /// When the trouble was seen.
    pub at: Instant,
    /// When the next attempt starts; `at` when it starts at once.
    pub next_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_delay(0, 0.5), Duration::from_millis(500));
        assert_eq!(backoff_delay(20, 0.5), BACKOFF_CAP);
    }

    #[test]
    fn retry_after_takes_integer_seconds_only() {
        assert_eq!(retry_after_secs("2"), Some(Duration::from_secs(2)));
        assert_eq!(retry_after_secs("soon"), None);
    }
}
