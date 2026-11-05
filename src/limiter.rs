use crate::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Longest single sleep while throttled, so a cancel is noticed promptly.
const CANCEL_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Token bucket shared by every connection of every download, capping their
/// combined speed. A limit of 0 means unlimited. The bucket holds up to one
/// second of traffic, so short bursts are allowed.
pub struct RateLimiter {
    bytes_per_sec: AtomicU64,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    refilled_at: Instant,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket {
                tokens: bytes_per_sec as f64,
                refilled_at: Instant::now(),
            }),
        }
    }

    pub fn bytes_per_sec(&self) -> u64 {
        self.bytes_per_sec.load(Ordering::Relaxed)
    }

    pub fn set_bytes_per_sec(&self, bytes_per_sec: u64) {
        self.bytes_per_sec.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Take `bytes` from the bucket and return how long the caller must wait
    /// before using them.
    pub fn reserve(&self, bytes: u64) -> Duration {
        self.reserve_at(bytes, Instant::now())
    }

    fn reserve_at(&self, bytes: u64, now: Instant) -> Duration {
        let rate = self.bytes_per_sec();
        if rate == 0 {
            return Duration::ZERO;
        }
        let rate = rate as f64;

        let mut bucket = self.bucket.lock().unwrap();
        let elapsed = now.saturating_duration_since(bucket.refilled_at);
        bucket.refilled_at = now;
        bucket.tokens = (bucket.tokens + elapsed.as_secs_f64() * rate).min(rate);
        bucket.tokens -= bytes as f64;

        if bucket.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-bucket.tokens / rate)
        }
    }
}

/// Wait until `bytes` may be written under `limiter`. Returns `Error::Cancelled`
/// if `cancel` fires while waiting.
pub async fn throttle(
    limiter: Option<&RateLimiter>,
    bytes: u64,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<(), Error> {
    let Some(limiter) = limiter else {
        return Ok(());
    };

    let mut remaining = limiter.reserve(bytes);
    while !remaining.is_zero() {
        if cancel.is_some_and(|rx| *rx.borrow()) {
            return Err(Error::Cancelled);
        }
        let step = remaining.min(CANCEL_CHECK_INTERVAL);
        tokio::time::sleep(step).await;
        remaining -= step;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let limiter = RateLimiter::new(0);
        assert_eq!(limiter.reserve(u64::MAX / 2), Duration::ZERO);
    }

    #[test]
    fn waits_once_the_one_second_burst_is_spent() {
        let limiter = RateLimiter::new(1000);
        let t0 = Instant::now();
        assert_eq!(limiter.reserve_at(1000, t0), Duration::ZERO);
        assert_eq!(limiter.reserve_at(500, t0), Duration::from_millis(500));
        // One second later the debt is repaid and 500 bytes are free again.
        assert_eq!(
            limiter.reserve_at(500, t0 + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn idle_time_refills_at_most_one_second_of_traffic() {
        let limiter = RateLimiter::new(1000);
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(60);
        assert_eq!(limiter.reserve_at(1000, later), Duration::ZERO);
        assert_eq!(limiter.reserve_at(1000, later), Duration::from_secs(1));
    }

    #[test]
    fn removing_the_limit_takes_effect_immediately() {
        let limiter = RateLimiter::new(1000);
        limiter.reserve(5000);
        limiter.set_bytes_per_sec(0);
        assert_eq!(limiter.reserve(5000), Duration::ZERO);
    }

    #[tokio::test]
    async fn throttle_stops_waiting_when_cancelled() {
        let limiter = RateLimiter::new(1000);
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();

        let start = std::time::Instant::now();
        let result = throttle(Some(&limiter), 60_000, Some(&rx)).await;
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
