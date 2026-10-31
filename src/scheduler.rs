use crate::ranges::{ByteRange, MIN_SEGMENT};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct Lane {
    pub cursor: Arc<AtomicU64>,
    pub end: Arc<AtomicU64>,
    pub started: Instant,
    pub bytes_done: Arc<AtomicU64>,
}

impl Lane {
    pub fn new(start: u64, end: u64, now: Instant) -> Self {
        Self {
            cursor: Arc::new(AtomicU64::new(start)),
            end: Arc::new(AtomicU64::new(end)),
            started: now,
            bytes_done: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub fn pick_victim(lanes: &[Lane], now: Instant) -> Option<usize> {
    let mut best_index = None;
    let mut max_eta = -1.0;

    for (i, lane) in lanes.iter().enumerate() {
        let cursor = lane.cursor.load(Ordering::SeqCst);
        let end = lane.end.load(Ordering::SeqCst);

        if end <= cursor || end - cursor < 2 * MIN_SEGMENT {
            continue;
        }

        let remaining = (end - cursor) as f64;
        let elapsed = now
            .saturating_duration_since(lane.started)
            .as_secs_f64()
            .max(0.001);
        let bytes_done = lane.bytes_done.load(Ordering::SeqCst) as f64;

        let speed = if bytes_done > 0.0 {
            bytes_done / elapsed
        } else {
            1.0
        };

        let eta = remaining / speed;
        if eta > max_eta {
            max_eta = eta;
            best_index = Some(i);
        }
    }

    best_index
}

pub fn steal(lane: &Lane) -> Option<ByteRange> {
    loop {
        let end = lane.end.load(Ordering::SeqCst);
        let cursor = lane.cursor.load(Ordering::SeqCst);

        if end <= cursor || end - cursor < 2 * MIN_SEGMENT {
            return None;
        }

        let midpoint = cursor + (end - cursor) / 2;

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
        };

        // Lane 1: remaining 8 MiB, done 2 MiB in 10s (speed 0.2 MiB/s -> ETA 40s)
        let l1 = Lane {
            cursor: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
            end: Arc::new(AtomicU64::new(10 * MIN_SEGMENT)),
            started,
            bytes_done: Arc::new(AtomicU64::new(2 * MIN_SEGMENT)),
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
        };

        let stolen = steal(&l).expect("steal should succeed");
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
        };

        assert_eq!(steal(&l), None);
    }

    #[test]
    fn repeated_steals_return_disjoint_ranges() {
        let l = Lane {
            cursor: Arc::new(AtomicU64::new(0)),
            end: Arc::new(AtomicU64::new(8 * MIN_SEGMENT)),
            started: Instant::now(),
            bytes_done: Arc::new(AtomicU64::new(0)),
        };

        let s1 = steal(&l).unwrap();
        assert_eq!(s1.start, 4 * MIN_SEGMENT);
        assert_eq!(s1.end, 8 * MIN_SEGMENT);

        let s2 = steal(&l).unwrap();
        assert_eq!(s2.start, 2 * MIN_SEGMENT);
        assert_eq!(s2.end, 4 * MIN_SEGMENT);

        assert!(s2.end <= s1.start);
    }
}
