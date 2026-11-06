use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the shown speed looks. Long enough to even out downloads that
/// take turns under a shared speed cap, short enough to follow real changes.
pub const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// A download's speed averaged over the last `SPEED_WINDOW`.
#[derive(Debug, Default)]
pub struct SpeedMeter {
    /// (when, total bytes on disk), oldest first.
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    /// Record the byte count at `at` and return the average speed in bytes per second.
    pub fn record(&mut self, at: Instant, bytes: u64) -> u64 {
        if self.samples.back().is_some_and(|&(_, last)| bytes < last) {
            // The count went backwards (a restart from zero): start over.
            self.samples.clear();
        }
        self.samples.push_back((at, bytes));
        // Keep one sample at or before the window start as the anchor.
        while self.samples.len() > 2
            && at.saturating_duration_since(self.samples[1].0) >= SPEED_WINDOW
        {
            self.samples.pop_front();
        }

        let (first_at, first_bytes) = self.samples[0];
        let elapsed = at.saturating_duration_since(first_at).as_secs_f64();
        if elapsed <= 0.0 {
            return 0;
        }
        ((bytes - first_bytes) as f64 / elapsed) as u64
    }

    /// Forget the history, for a download that is starting again.
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_single_sample_has_no_speed() {
        let mut meter = SpeedMeter::default();
        assert_eq!(meter.record(Instant::now(), 5000), 0);
    }

    #[test]
    fn bursts_that_take_turns_average_out() {
        // 1 MiB arrives every other 250 ms tick, and nothing in between.
        let mut meter = SpeedMeter::default();
        let t0 = Instant::now();
        let mut bytes = 0;
        let mut speeds = Vec::new();
        for tick in 0..=16u64 {
            if tick % 2 == 1 {
                bytes += 1024 * 1024;
            }
            speeds.push(meter.record(t0 + ms(250 * tick), bytes));
        }
        // Once the window has filled, every reading is close to the true 2 MiB/s.
        for speed in &speeds[12..] {
            let off = (*speed as f64 - 2.0 * 1024.0 * 1024.0).abs() / (2.0 * 1024.0 * 1024.0);
            assert!(off < 0.15, "{speeds:?}");
        }
    }

    #[test]
    fn old_samples_drop_out_of_the_window() {
        let mut meter = SpeedMeter::default();
        let t0 = Instant::now();
        meter.record(t0, 0);
        // A fast first second, then 4 s at 1000 bytes/s: the fast start no longer counts.
        meter.record(t0 + ms(1000), 10_000_000);
        let mut speed = 0;
        for s in 1..=4u64 {
            speed = meter.record(t0 + ms(1000 + 1000 * s), 10_000_000 + 1000 * s);
        }
        assert_eq!(speed, 1000);
    }

    #[test]
    fn a_stall_reads_as_zero_once_the_window_passes() {
        let mut meter = SpeedMeter::default();
        let t0 = Instant::now();
        meter.record(t0, 0);
        meter.record(t0 + ms(500), 50_000);
        let mut speed = 1;
        for s in 1..=4u64 {
            speed = meter.record(t0 + ms(500 + 1000 * s), 50_000);
        }
        assert_eq!(speed, 0);
    }

    #[test]
    fn a_count_that_goes_backwards_starts_over() {
        let mut meter = SpeedMeter::default();
        let t0 = Instant::now();
        meter.record(t0, 0);
        meter.record(t0 + ms(1000), 4096);
        assert_eq!(meter.record(t0 + ms(1250), 0), 0);
        assert_eq!(meter.record(t0 + ms(2250), 1000), 1000);
    }
}
