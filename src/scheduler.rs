use crate::ranges::{ByteRange, MIN_SEGMENT};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Smallest piece a slow lane is left with when an idle connection helps it near the end.
pub const ENDGAME_MIN_PIECE: u64 = 32 * 1024;
/// A lane's speed is trusted once it has been receiving data this long...
pub const ENDGAME_SAMPLE: Duration = Duration::from_millis(250);
/// ...and a lane that has received nothing counts as stalled after this long.
pub const ENDGAME_STALL: Duration = Duration::from_secs(2);
/// A lane too small for an ordinary split is helped only when it would still need
/// longer than this on its own...
pub const ENDGAME_MIN_ETA: Duration = Duration::from_secs(1);
/// ...and then keeps only what it can write in this long; the helper takes the rest.
pub const ENDGAME_KEEP: Duration = Duration::from_millis(500);

/// The connections of one download that are still at work.
pub struct Crew {
    active: AtomicUsize,
}

impl Crew {
    pub fn new(connections: usize) -> Self {
        Self {
            active: AtomicUsize::new(connections),
        }
    }

    /// True while some other connection is still at work.
    pub fn others_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 1
    }

    /// Leave the crew unless this is its last connection, which must stay to finish the file.
    pub fn try_leave(&self) -> bool {
        self.decrement_while(|n| n > 1)
    }

    /// Leave the crew for good: this connection has nothing more to do.
    pub fn leave(&self) {
        self.decrement_while(|n| n > 0);
    }

    fn decrement_while(&self, allowed: impl Fn(usize) -> bool) -> bool {
        let mut n = self.active.load(Ordering::SeqCst);
        while allowed(n) {
            match self
                .active
                .compare_exchange_weak(n, n - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        false
    }
}

#[derive(Clone)]
pub struct Lane {
    pub cursor: Arc<AtomicU64>,
    pub end: Arc<AtomicU64>,
    pub started: Instant,
    pub bytes_done: Arc<AtomicU64>,
    /// When the lane's first byte arrived.
    pub first_byte: Arc<OnceLock<Instant>>,
}

impl Lane {
    pub fn new(start: u64, end: u64, now: Instant) -> Self {
        Self {
            cursor: Arc::new(AtomicU64::new(start)),
            end: Arc::new(AtomicU64::new(end)),
            started: now,
            bytes_done: Arc::new(AtomicU64::new(0)),
            first_byte: Arc::new(OnceLock::new()),
        }
    }

    /// Record that data is arriving; only the first call counts.
    pub fn mark_first_byte(&self) {
        self.first_byte.get_or_init(Instant::now);
    }

    /// True once every byte of this lane is written (or handed elsewhere).
    pub fn is_done(&self) -> bool {
        self.cursor.load(Ordering::SeqCst) >= self.end.load(Ordering::SeqCst)
    }

    /// Take back the part of this lane not yet written, leaving the lane done. Only
    /// the lane's own connection calls this, so its cursor cannot move meanwhile.
    pub fn take_rest(&self) -> Option<ByteRange> {
        loop {
            let end = self.end.load(Ordering::SeqCst);
            let cursor = self.cursor.load(Ordering::SeqCst);
            if end <= cursor {
                return None;
            }
            if self
                .end
                .compare_exchange_weak(end, cursor, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(ByteRange { start: cursor, end });
            }
        }
    }

    /// Bytes a second since the first byte arrived; a lane with nothing yet counts
    /// as one byte a second.
    fn speed(&self, now: Instant) -> f64 {
        let done = self.bytes_done.load(Ordering::SeqCst) as f64;
        if done == 0.0 {
            return 1.0;
        }
        let since = self.first_byte.get().copied().unwrap_or(self.started);
        done / now
            .saturating_duration_since(since)
            .as_secs_f64()
            .max(0.001)
    }

    /// True once the lane's speed can be judged: data has been arriving for a while,
    /// or nothing has arrived for long enough to call it stalled.
    fn speed_is_known(&self, now: Instant) -> bool {
        match self.first_byte.get() {
            Some(first) => now.saturating_duration_since(*first) >= ENDGAME_SAMPLE,
            None => now.saturating_duration_since(self.started) >= ENDGAME_STALL,
        }
    }

    /// Where another connection may split this lane, and the seconds the lane would
    /// need on its own. Big lanes are halved. Near the end, a lane that has proved slow
    /// or stalled keeps only what it can write in [`ENDGAME_KEEP`].
    fn split_point(&self, cursor: u64, end: u64, now: Instant) -> Option<(u64, f64)> {
        let remaining = end.saturating_sub(cursor);
        let speed = self.speed(now);
        let eta = remaining as f64 / speed;
        if remaining >= 2 * MIN_SEGMENT {
            return Some((cursor + remaining / 2, eta));
        }
        if remaining < 2 * ENDGAME_MIN_PIECE
            || eta <= ENDGAME_MIN_ETA.as_secs_f64()
            || !self.speed_is_known(now)
        {
            return None;
        }
        let keep =
            ((speed * ENDGAME_KEEP.as_secs_f64()) as u64).clamp(ENDGAME_MIN_PIECE, remaining / 2);
        Some((cursor + keep, eta))
    }
}

pub fn pick_victim(lanes: &[Lane], now: Instant) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, lane) in lanes.iter().enumerate() {
        let cursor = lane.cursor.load(Ordering::SeqCst);
        let end = lane.end.load(Ordering::SeqCst);
        let Some((_, eta)) = lane.split_point(cursor, end, now) else {
            continue;
        };
        if best.is_none_or(|(_, max_eta)| eta > max_eta) {
            best = Some((i, eta));
        }
    }
    best.map(|(i, _)| i)
}

pub fn steal(lane: &Lane, now: Instant) -> Option<ByteRange> {
    loop {
        let end = lane.end.load(Ordering::SeqCst);
        let cursor = lane.cursor.load(Ordering::SeqCst);

        let (midpoint, _) = lane.split_point(cursor, end, now)?;

        if lane
            .end
            .compare_exchange_weak(end, midpoint, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(ByteRange {
                start: midpoint,
                end,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pick_victim_chooses_longest_eta() {
        let now = Instant::now();
        let started = now - Duration::from_secs(10);

        // Lane 0: remaining 4 MiB, done 2 MiB in 10s (speed 0.2 MiB/s -> ETA 20s)
        let l0 = Lane {
            cursor: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
            end: Arc::new(AtomicU64::new(6 * MIN_SEGMENT)),
            started,
            bytes_done: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
            first_byte: Arc::new(OnceLock::new()),
        };

        // Lane 1: remaining 8 MiB, done 2 MiB in 10s (speed 0.2 MiB/s -> ETA 40s)
        let l1 = Lane {
            cursor: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
            end: Arc::new(AtomicU64::new(10 * MIN_SEGMENT)),
            started,
            bytes_done: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
            first_byte: Arc::new(OnceLock::new()),
        };

        assert_eq!(pick_victim(&[l0, l1], now), Some(1));
    }

    #[test]
    fn steal_halves_remaining_and_lowers_victim_end() {
        let l = Lane {
            cursor: Arc::new(AtomicU64::new(0)),
            end: Arc::new(AtomicU64::new(4 * MIN_SEGMENT)),
            started: Instant::now(),
            bytes_done: Arc::new(AtomicU64::new(0)),
            first_byte: Arc::new(OnceLock::new()),
        };

        let stolen = steal(&l, Instant::now()).expect("steal should succeed");
        assert_eq!(stolen.start, 2 * MIN_SEGMENT);
        assert_eq!(stolen.end, 4 * MIN_SEGMENT);
        assert_eq!(l.end.load(Ordering::SeqCst), 2 * MIN_SEGMENT);
    }

    #[test]
    fn steal_refuses_below_threshold() {
        let l = Lane {
            cursor: Arc::new(AtomicU64::new(0)),
            end: Arc::new(AtomicU64::new(2 * MIN_SEGMENT - 1)),
            started: Instant::now(),
            bytes_done: Arc::new(AtomicU64::new(0)),
            first_byte: Arc::new(OnceLock::new()),
        };

        assert_eq!(steal(&l, Instant::now()), None);
    }

    #[test]
    fn repeated_steals_return_disjoint_ranges() {
        let l = Lane {
            cursor: Arc::new(AtomicU64::new(0)),
            end: Arc::new(AtomicU64::new(8 * MIN_SEGMENT)),
            started: Instant::now(),
            bytes_done: Arc::new(AtomicU64::new(0)),
            first_byte: Arc::new(OnceLock::new()),
        };

        let s1 = steal(&l, Instant::now()).unwrap();
        assert_eq!(s1.start, 4 * MIN_SEGMENT);
        assert_eq!(s1.end, 8 * MIN_SEGMENT);

        let s2 = steal(&l, Instant::now()).unwrap();
        assert_eq!(s2.start, 2 * MIN_SEGMENT);
        assert_eq!(s2.end, 4 * MIN_SEGMENT);

        assert!(s2.end <= s1.start);
    }

    /// A lane opened `opened` ago, whose first byte came `first_byte` ago, with
    /// `done` bytes written and `remaining` to go.
    fn lane_at(
        remaining: u64,
        done: u64,
        opened: Duration,
        first_byte: Option<Duration>,
        now: Instant,
    ) -> Lane {
        let first = OnceLock::new();
        if let Some(ago) = first_byte {
            let _ = first.set(now - ago);
        }
        Lane {
            cursor: Arc::new(AtomicU64::new(done)),
            end: Arc::new(AtomicU64::new(done + remaining)),
            started: now - opened,
            bytes_done: Arc::new(AtomicU64::new(done)),
            first_byte: Arc::new(first),
        }
    }

    /// A lane that has written `done` bytes in `ran`, data arriving from the start.
    fn small_lane(remaining: u64, done: u64, ran: Duration, now: Instant) -> Lane {
        let first_byte = (done > 0).then_some(ran);
        lane_at(remaining, done, ran, first_byte, now)
    }

    #[test]
    fn a_slow_lane_near_the_end_keeps_only_what_it_can_write_in_half_a_second() {
        let now = Instant::now();
        // 512 KiB left at 128 KiB/s: 4 s to go.
        let l = small_lane(512 * 1024, 256 * 1024, Duration::from_secs(2), now);
        let stolen = steal(&l, now).expect("a slow tail should be split");
        assert_eq!(stolen.start, 256 * 1024 + 64 * 1024);
        assert_eq!(stolen.end, 768 * 1024);
        assert_eq!(
            pick_victim(&[l], now),
            None,
            "64 KiB at 128 KiB/s is under a second"
        );
    }

    #[test]
    fn a_helper_takes_at_least_half_of_a_lane_it_helps() {
        let now = Instant::now();
        // 1900 KiB left at 1.5 MiB/s: about 1.24 s to go.
        let l = small_lane(1900 * 1024, 3 * MIN_SEGMENT, Duration::from_secs(2), now);
        let stolen = steal(&l, now).expect("over a second to go");
        assert!(
            stolen.len() >= 1900 * 1024 / 2,
            "took {} bytes",
            stolen.len()
        );
    }

    #[test]
    fn a_small_lane_is_left_alone_while_it_is_fast_or_its_speed_unknown() {
        let now = Instant::now();
        // 512 KiB left at 8 MiB/s: done in well under a second.
        let fast = small_lane(512 * 1024, 16 * MIN_SEGMENT, Duration::from_secs(2), now);
        assert_eq!(steal(&fast, now), None);
        // Opened 100 ms ago, nothing yet.
        let new = small_lane(512 * 1024, 0, Duration::from_millis(100), now);
        assert_eq!(steal(&new, now), None);
        // Slow to connect (3 s), and its first byte only 100 ms ago.
        let late = lane_at(
            512 * 1024,
            8 * 1024,
            Duration::from_secs(3),
            Some(Duration::from_millis(100)),
            now,
        );
        assert_eq!(steal(&late, now), None);
        assert_eq!(pick_victim(&[fast, new, late], now), None);
    }

    #[test]
    fn a_lane_that_receives_nothing_is_split_once_it_has_stalled() {
        let now = Instant::now();
        let waiting = lane_at(512 * 1024, 0, Duration::from_secs(1), None, now);
        assert_eq!(steal(&waiting, now), None, "a slow server may still answer");
        let stalled = lane_at(512 * 1024, 0, ENDGAME_STALL, None, now);
        let stolen = steal(&stalled, now).expect("a stalled lane should be helped");
        assert_eq!(stolen.start, ENDGAME_MIN_PIECE);
    }

    #[test]
    fn a_crawling_lane_is_judged_soon_after_its_first_byte() {
        let now = Instant::now();
        // 32 KiB in the 300 ms since its first byte, 1 MiB to go.
        let crawling = lane_at(
            MIN_SEGMENT,
            32 * 1024,
            Duration::from_millis(400),
            Some(Duration::from_millis(300)),
            now,
        );
        assert!(steal(&crawling, now).is_some());
    }

    #[test]
    fn a_slow_tail_is_not_split_into_pieces_smaller_than_the_endgame_minimum() {
        let now = Instant::now();
        let l = small_lane(2 * ENDGAME_MIN_PIECE - 1, 1024, Duration::from_secs(5), now);
        assert_eq!(steal(&l, now), None);
    }

    #[test]
    fn take_rest_hands_back_the_unwritten_part_and_leaves_the_lane_done() {
        let l = small_lane(3 * MIN_SEGMENT, MIN_SEGMENT, Duration::ZERO, Instant::now());
        let rest = l.take_rest().unwrap();
        assert_eq!((rest.start, rest.end), (MIN_SEGMENT, 4 * MIN_SEGMENT));
        assert!(l.is_done());
        assert_eq!(l.take_rest(), None);
    }

    #[test]
    fn the_last_connection_of_a_crew_cannot_leave_early() {
        let crew = Crew::new(2);
        assert!(crew.others_active());
        assert!(crew.try_leave());
        assert!(!crew.others_active());
        assert!(!crew.try_leave());
        crew.leave();
        crew.leave(); // never wraps below zero
        assert!(!crew.try_leave());
    }
}
