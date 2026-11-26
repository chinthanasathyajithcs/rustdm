//! What a pieces stream is doing, for the row and the details pane: how many
//! pieces are done, which wait to be written, and how far each running one is.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// One piece being fetched: the bytes so far, of how many when the server said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunningPiece {
    pub got: u64,
    pub len: Option<u64>,
}

/// A copy of where the stream's pieces are, at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PieceActivity {
    /// Pieces `0..written` are in the stream file.
    pub written: usize,
    /// Pieces finished early, waiting for the ones before them.
    pub waiting: BTreeSet<usize>,
    pub running: BTreeMap<usize, RunningPiece>,
}

/// How many pieces are finished, of how many (the row's `piece 140 of 312`),
/// and what each piece is doing (the details pane).
#[derive(Debug, Default)]
pub struct PieceCounter {
    pub done: AtomicU64,
    pub total: AtomicU64,
    activity: Mutex<PieceActivity>,
}

impl PieceCounter {
    /// Where the pieces are now.
    pub fn activity(&self) -> PieceActivity {
        self.activity.lock().unwrap().clone()
    }

    /// The pieces on disk: `written` in the stream file and `waiting` beside it.
    pub(super) fn kept(&self, written: usize, waiting: BTreeSet<usize>) {
        self.done
            .store((written + waiting.len()) as u64, Ordering::Relaxed);
        let mut activity = self.activity.lock().unwrap();
        activity.written = written;
        activity.waiting = waiting;
    }

    /// Piece `index` starts (again) from its first byte.
    pub(super) fn start(&self, index: usize) {
        let mut activity = self.activity.lock().unwrap();
        activity.running.insert(index, RunningPiece::default());
    }

    /// Piece `index` got `bytes` more, of `len` when known.
    pub(super) fn grow(&self, index: usize, bytes: u64, len: Option<u64>) {
        let mut activity = self.activity.lock().unwrap();
        if let Some(piece) = activity.running.get_mut(&index) {
            piece.got += bytes;
            piece.len = len.or(piece.len);
        }
    }

    /// Piece `index` is no longer being fetched: finished, or stopped.
    pub(super) fn stop(&self, index: usize) {
        self.activity.lock().unwrap().running.remove(&index);
    }

    /// No piece is being fetched any more.
    pub(super) fn stop_all(&self) {
        self.activity.lock().unwrap().running.clear();
    }

    #[cfg(test)]
    pub fn set_activity_for_tests(&self, activity: PieceActivity) {
        *self.activity.lock().unwrap() = activity;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_pieces_count_as_done() {
        let counter = PieceCounter::default();

        counter.kept(3, BTreeSet::from([5, 7]));

        assert_eq!(counter.done.load(Ordering::Relaxed), 5);
        let activity = counter.activity();
        assert_eq!(activity.written, 3);
        assert_eq!(activity.waiting, BTreeSet::from([5, 7]));
    }

    #[test]
    fn a_running_piece_grows_and_starts_over_when_retried() {
        let counter = PieceCounter::default();

        counter.start(4);
        counter.grow(4, 100, Some(1_000));
        counter.grow(4, 50, None);
        assert_eq!(
            counter.activity().running[&4],
            RunningPiece {
                got: 150,
                len: Some(1_000)
            }
        );

        counter.start(4);
        assert_eq!(counter.activity().running[&4], RunningPiece::default());
    }

    #[test]
    fn stopped_pieces_leave_the_running_list() {
        let counter = PieceCounter::default();
        counter.start(1);
        counter.start(2);
        // A piece that is not running does not appear by growing.
        counter.grow(9, 10, None);

        counter.stop(1);
        assert_eq!(counter.activity().running.keys().collect::<Vec<_>>(), [&2]);
        counter.stop_all();
        assert!(counter.activity().running.is_empty());
    }
}
