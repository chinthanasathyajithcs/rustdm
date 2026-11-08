//! Splitting the overall speed cap between running downloads.
//!
//! Every running download gets its own bucket. Its rate is an equal share of
//! the cap, except that a download which cannot use its share (a slow server,
//! or its own lower cap) gets about what it uses, and the rest goes to the
//! others. Without this, downloads that started first or open more connections
//! take most of a single shared bucket.

/// Extra room a download that is not using its whole share keeps, so it can
/// speed up again.
const HEADROOM: u64 = 64 * 1024;

/// What a running download should be offered: `None` when it would take more
/// than it has (it is using at least 80% of its share, or is too new to judge),
/// otherwise its current speed plus room to grow. Never more than `own_cap`
/// when that is set (non-zero).
pub fn demand(speed: u64, share: u64, own_cap: u64, measured: bool) -> Option<u64> {
    let wants_more = !measured || share == 0 || speed.saturating_mul(10) >= share.saturating_mul(8);
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

    #[test]
    fn a_download_never_needs_more_than_its_own_cap() {
        assert_eq!(demand(900, 1000, 500, true), Some(500));
        assert_eq!(demand(100_000, 1_000_000, 40_000, true), Some(40_000));
    }
}
