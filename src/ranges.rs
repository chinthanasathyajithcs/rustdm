pub const MIN_SEGMENT: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet {
    ranges: Vec<ByteRange>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    pub fn insert(&mut self, mut r: ByteRange) {
        if r.is_empty() {
            return;
        }

        let mut new_ranges = Vec::with_capacity(self.ranges.len() + 1);
        let mut inserted = false;

        for existing in &self.ranges {
            if inserted {
                new_ranges.push(*existing);
            } else if r.end < existing.start {
                new_ranges.push(r);
                new_ranges.push(*existing);
                inserted = true;
            } else if r.start > existing.end {
                new_ranges.push(*existing);
            } else {
                r.start = r.start.min(existing.start);
                r.end = r.end.max(existing.end);
            }
        }

        if !inserted {
            new_ranges.push(r);
        }

        self.ranges = new_ranges;
    }

    pub fn ranges(&self) -> &[ByteRange] {
        &self.ranges
    }

    pub fn covered(&self) -> u64 {
        self.ranges.iter().map(|r| r.len()).sum()
    }

    pub fn complement(&self, total: u64) -> Vec<ByteRange> {
        if total == 0 {
            return Vec::new();
        }

        let mut missing = Vec::new();
        let mut current = 0;

        for r in &self.ranges {
            if current < r.start {
                let end = r.start.min(total);
                if current < end {
                    missing.push(ByteRange {
                        start: current,
                        end,
                    });
                }
            }
            current = current.max(r.end);
            if current >= total {
                break;
            }
        }

        if current < total {
            missing.push(ByteRange {
                start: current,
                end: total,
            });
        }

        missing
    }
}

pub fn initial_segments(total: u64, connections: u32) -> Vec<ByteRange> {
    if total == 0 || connections == 0 {
        return Vec::new();
    }

    let count = (total / MIN_SEGMENT).min(connections as u64).max(1);
    let seg_size = total / count;
    let mut segments = Vec::with_capacity(count as usize);

    for i in 0..count {
        let start = i * seg_size;
        let end = if i == count - 1 {
            total
        } else {
            (i + 1) * seg_size
        };
        segments.push(ByteRange { start, end });
    }

    segments
}

pub fn split_upper_half(r: ByteRange) -> Option<(ByteRange, ByteRange)> {
    if r.len() < 2 * MIN_SEGMENT {
        return None;
    }
    let mid = r.start + r.len() / 2;
    Some((
        ByteRange {
            start: r.start,
            end: mid,
        },
        ByteRange {
            start: mid,
            end: r.end,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> ByteRange {
        ByteRange { start, end }
    }

    #[test]
    fn insert_merges_adjacent_and_overlapping() {
        let mut s = RangeSet::new();
        s.insert(r(0, 5));
        s.insert(r(5, 10));
        s.insert(r(8, 12));
        assert_eq!(s.ranges(), &[r(0, 12)]);
    }

    #[test]
    fn complement_lists_missing_gaps() {
        let mut s = RangeSet::new();
        s.insert(r(0, 5));
        s.insert(r(10, 15));
        assert_eq!(s.complement(20), vec![r(5, 10), r(15, 20)]);
    }

    #[test]
    fn initial_segments_capped_by_connections_and_size() {
        let segs = initial_segments(8 * MIN_SEGMENT, 4);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs.last().unwrap().end, 8 * MIN_SEGMENT); // last absorbs remainder
    }

    #[test]
    fn small_file_gets_one_segment() {
        assert_eq!(initial_segments(100, 8), vec![r(0, 100)]);
    }

    #[test]
    fn zero_length_has_no_segments() {
        assert!(initial_segments(0, 8).is_empty());
    }

    #[test]
    fn split_needs_two_min_segments() {
        assert_eq!(split_upper_half(r(0, 2 * MIN_SEGMENT - 1)), None);
        assert_eq!(
            split_upper_half(r(0, 2 * MIN_SEGMENT)),
            Some((r(0, MIN_SEGMENT), r(MIN_SEGMENT, 2 * MIN_SEGMENT)))
        );
    }
}
