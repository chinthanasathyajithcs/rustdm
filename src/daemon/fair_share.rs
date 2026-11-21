//! Splitting the overall speed cap between running downloads.
//!
//! Every running download gets its own bucket. Its rate is an equal share of
//! the cap, except that a download which cannot use its share (a slow server,
//! or its own lower cap) gets about what it uses, and the rest goes to the
//! others. Without this, downloads that started first or open more connections
//! take most of a single shared bucket.

use std::time::Duration;

/// Extra room a download that is not using its whole share keeps, so it can
/// speed up again.
const HEADROOM: u64 = 64 * 1024;

/// A download whose share bucket made it wait this recently is held back by
/// its share, whatever its average speed says (two telemetry ticks).
pub const HELD_BACK_WINDOW: Duration = Duration::from_millis(500);

/// What a running download should be offered: `None` when it would take more
/// than it has (it is using at least 80% of its share, or its speed says
/// nothing yet), otherwise its current speed plus room to grow. Never more than
/// `own_cap` when that is set (non-zero).
///
/// `judged_by_speed` is false while the speed cannot be trusted to show what
/// the download needs: it is too new, or its share held it back just now. The
/// speed is an average over seconds, so after a stall it trails far behind;
/// offering only that average plus a little would keep the download from ever
/// catching up with its share.
pub fn demand(speed: u64, share: u64, own_cap: u64, judged_by_speed: bool) -> Option<u64> {
    let wants_more =
        !judged_by_speed || share == 0 || speed.saturating_mul(10) >= share.saturating_mul(8);
    let want = (!wants_more).then(|| speed + speed / 2 + HEADROOM);
    match (want, own_cap) {
        (want, 0) => want,
        (Some(want), cap) => Some(want.min(cap)),
        (None, cap) => Some(cap),
    }
}

/// Split `total` bytes per second between downloads with these `demands`
/// (max-min fairness): small demands are met in full, the rest share what is
/// left equally. If every demand is met, what is left over is spread evenly so
/// no one is held below the cap for nothing. A `total` of 0 means no cap.
pub fn fair_shares(total: u64, demands: &[Option<u64>]) -> Vec<u64> {
    let n = demands.len();
    if total == 0 || n == 0 {
        return vec![0; n];
    }

    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| demands[i].unwrap_or(u64::MAX));

    let mut shares = vec![0; n];
    let mut remaining = total;
    for (k, &i) in order.iter().enumerate() {
        let equal = remaining / (n - k) as u64;
        let give = demands[i].map_or(equal, |d| d.min(equal));
        shares[i] = give;
        remaining -= give;
    }

    let extra = remaining / n as u64;
    shares.iter().map(|s| s + extra).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn hungry_downloads_split_the_cap_equally() {
        assert_eq!(fair_shares(3 * MIB, &[None, None, None]), vec![MIB; 3]);
    }

    #[test]
    fn a_download_that_needs_less_leaves_the_rest_to_others() {
        let shares = fair_shares(3 * MIB, &[Some(MIB / 4), None, None]);
        assert_eq!(shares[0], MIB / 4);
        assert_eq!(shares[1], shares[2]);
        assert_eq!(shares.iter().sum::<u64>(), 3 * MIB);
    }

    #[test]
    fn leftovers_are_spread_when_everyone_is_satisfied() {
        let shares = fair_shares(3 * MIB, &[Some(MIB / 2), Some(MIB / 2)]);
        assert_eq!(shares, vec![MIB / 2 + MIB, MIB / 2 + MIB]);
    }

    #[test]
    fn no_cap_means_no_shares() {
        assert_eq!(fair_shares(0, &[None, Some(5)]), vec![0, 0]);
        assert!(fair_shares(MIB, &[]).is_empty());
    }

    #[test]
    fn demand_depends_on_how_much_of_the_share_is_used() {
        // Using 90% of its share: wants more.
        assert_eq!(demand(900, 1000, 0, true), None);
        // Using 10%: offered its speed plus headroom.
        assert_eq!(
            demand(100_000, 1_000_000, 0, true),
            Some(150_000 + HEADROOM)
        );
        // Too new to judge, or no cap in force: wants more.
        assert_eq!(demand(0, 1000, 0, false), None);
        assert_eq!(demand(5, 0, 0, true), None);
    }

    /// Three greedy downloads share a 3 MiB/s cap, as the daemon runs them: each
    /// takes from its own share bucket as fast as the bucket allows, and every
    /// 250 ms tick measures the speeds and hands out new shares. Download 0
    /// gets nothing from its server between `stall`; the result is how many
    /// bytes each download got from `from` to `to` seconds.
    fn simulate(stall: (f64, f64), from: f64, to: f64) -> Vec<u64> {
        use crate::daemon::speed::{SpeedMeter, SPEED_WINDOW};
        use crate::limiter::RateLimiter;

        const STEP_MS: u64 = 5;
        const TICK_MS: u64 = 250;
        const CHUNK: u64 = 16 * 1024;
        let t0 = tokio::time::Instant::now();
        let clock0 = std::time::Instant::now();
        let limiters: Vec<RateLimiter> = (0..3).map(|_| RateLimiter::new(MIB)).collect();
        let mut meters: Vec<SpeedMeter> = (0..3).map(|_| SpeedMeter::default()).collect();
        let mut ready_at = [0u64; 3];
        let mut bytes = [0u64; 3];
        let (mut at_from, mut at_to) = ([0u64; 3], [0u64; 3]);

        for ms in (0..=(to * 1000.0) as u64).step_by(STEP_MS as usize) {
            let secs = ms as f64 / 1000.0;
            let now = t0 + Duration::from_millis(ms);
            for i in 0..3 {
                let stalled = i == 0 && secs >= stall.0 && secs < stall.1;
                if stalled || ms < ready_at[i] {
                    continue;
                }
                let wait = limiters[i].reserve_at(CHUNK, now);
                bytes[i] += CHUNK;
                ready_at[i] = ms + wait.as_millis() as u64;
            }
            if ms % TICK_MS == 0 {
                let clock = clock0 + Duration::from_millis(ms);
                let demands: Vec<_> = (0..3)
                    .map(|i| {
                        let speed = meters[i].record(clock, bytes[i]);
                        let share = limiters[i].bytes_per_sec();
                        let settled = Duration::from_millis(ms) >= SPEED_WINDOW;
                        let held_back = limiters[i].held_back_at(now, HELD_BACK_WINDOW);
                        demand(speed, share, 0, settled && !held_back)
                    })
                    .collect();
                for (limiter, share) in limiters.iter().zip(fair_shares(3 * MIB, &demands)) {
                    limiter.set_bytes_per_sec(share);
                }
            }
            if ms == (from * 1000.0) as u64 {
                at_from = bytes;
            }
            at_to = bytes;
        }
        (0..3).map(|i| at_to[i] - at_from[i]).collect()
    }

    #[test]
    fn a_download_that_stalled_gets_its_share_back_within_a_second() {
        // Stalled from 5 s to 7.5 s; measured from 8.5 s to 11.5 s.
        let got = simulate((5.0, 7.5), 8.5, 11.5);
        let rates: Vec<f64> = got.iter().map(|b| *b as f64 / 3.0 / MIB as f64).collect();
        for rate in &rates {
            assert!(
                (0.9..=1.1).contains(rate),
                "each should be back to about 1 MiB/s, got {rates:.2?}"
            );
        }
    }

    #[test]
    fn a_download_never_needs_more_than_its_own_cap() {
        assert_eq!(demand(900, 1000, 500, true), Some(500));
        assert_eq!(demand(100_000, 1_000_000, 40_000, true), Some(40_000));
    }
}
