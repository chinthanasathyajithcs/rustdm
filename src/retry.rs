use std::time::Duration;

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
