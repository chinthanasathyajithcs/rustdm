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
    /// The last time a caller had to wait for this bucket.
    waited_at: Option<Instant>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket {
                tokens: bytes_per_sec as f64,
                refilled_at: Instant::now(),
                waited_at: None,
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

    pub(crate) fn reserve_at(&self, bytes: u64, now: Instant) -> Duration {
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
            bucket.waited_at = Some(now);
            Duration::from_secs_f64(-bucket.tokens / rate)
        }
    }

    /// Whether this bucket made a caller wait within the last `window`: the
    /// limit, not the server, is what decides the speed.
    pub fn held_back_within(&self, window: Duration) -> bool {
        self.held_back_at(Instant::now(), window)
    }

    pub(crate) fn held_back_at(&self, now: Instant, window: Duration) -> bool {
        let bucket = self.bucket.lock().unwrap();
        bucket
            .waited_at
            .is_some_and(|at| now.saturating_duration_since(at) <= window)
    }
}

/// Wait until `bytes` may be written under every one of `limiters`: the bytes
/// are taken from each bucket and the caller waits for the slowest. Returns
/// `Error::Cancelled` if `cancel` fires while waiting.
pub async fn throttle_all<'a>(
    limiters: impl IntoIterator<Item = &'a RateLimiter>,
    bytes: u64,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<(), Error> {
    let mut remaining = limiters
        .into_iter()
        .map(|limiter| limiter.reserve(bytes))
        .max()
        .unwrap_or(Duration::ZERO);
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
    fn a_bucket_says_whether_it_held_a_caller_back_lately() {
        let limiter = RateLimiter::new(1000);
        let t0 = Instant::now();
        let window = Duration::from_millis(500);
        assert_eq!(limiter.reserve_at(1000, t0), Duration::ZERO);
        assert!(!limiter.held_back_at(t0, window), "it had the bytes");

        assert!(limiter.reserve_at(500, t0) > Duration::ZERO);
        assert!(limiter.held_back_at(t0 + window, window));
        assert!(
            !limiter.held_back_at(t0 + window * 2, window),
            "too long ago"
        );
    }

    #[test]
    fn removing_the_limit_takes_effect_immediately() {
        let limiter = RateLimiter::new(1000);
        limiter.reserve(5000);
        limiter.set_bytes_per_sec(0);
        assert_eq!(limiter.reserve(5000), Duration::ZERO);
    }

    #[tokio::test]
    async fn throttle_all_waits_for_the_slowest_limiter() {
        let shared = RateLimiter::new(0);
        let own = RateLimiter::new(100_000);
        own.reserve(100_000);

        let start = std::time::Instant::now();
        throttle_all([&shared, &own], 20_000, None).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(190));
        // The unlimited bucket never holds anyone back.
        assert_eq!(shared.reserve(u64::MAX / 2), Duration::ZERO);
    }

    #[tokio::test]
    async fn throttle_stops_waiting_when_cancelled() {
        let limiter = RateLimiter::new(1000);
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();

        let start = std::time::Instant::now();
        let result = throttle_all([&limiter], 60_000, Some(&rx)).await;
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
