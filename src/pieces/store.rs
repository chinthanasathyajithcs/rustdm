//! Where a pieces stream keeps its work: the in-order `.part`, the state file,
//! and the folder of pieces finished early (spec 1c, sections 7.2, 7.3, 7.5).

use super::Piece;
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;
const TEMP_SUFFIX: &str = ".tmp";

/// `X.part.pieces`: how far the pieces of stream file `X` are written.
pub fn state_path(file: &Path) -> PathBuf {
    with_suffix(file, ".part.pieces")
}

/// `X.pieces`: pieces of `X` finished before the ones in front of them.
pub fn waiting_dir(file: &Path) -> PathBuf {
    with_suffix(file, ".pieces")
}

/// Every file and folder a pieces stream may leave beside `file`.
pub fn leftovers(file: &Path) -> Vec<PathBuf> {
    vec![part_path(file), state_path(file), waiting_dir(file)]
}

fn part_path(file: &Path) -> PathBuf {
    with_suffix(file, ".part")
}

fn with_suffix(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// A digest of what the pieces are, never where: the count, and each piece's
/// duration and byte range. Fresh signed links leave it unchanged.
pub fn fingerprint(pieces: &[Piece]) -> String {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(&(pieces.len() as u64).to_le_bytes());
    for piece in pieces {
        digest.update(&piece.duration_ms.to_le_bytes());
        match piece.range {
            Some(slice) => {
                digest.update(&[1]);
                digest.update(&slice.offset.to_le_bytes());
                digest.update(&slice.len.to_le_bytes());
            }
            None => digest.update(&[0]),
        }
    }
    digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// May piece `index` start while `first_unwritten` is still missing? Not more
/// than twice the connections ahead, so the waiting folder stays small.
pub fn may_start(index: usize, first_unwritten: usize, connections: u32) -> bool {
    index < first_unwritten + 2 * connections.max(1) as usize
}

#[derive(Serialize, Deserialize)]
struct SavedState {
    version: u32,
    fingerprint: String,
    count: usize,
    written: usize,
    bytes: u64,
}

/// The work of one pieces stream on disk.
#[derive(Debug)]
pub struct PieceStore {
    file: PathBuf,
    count: usize,
    fingerprint: String,
    written: usize,
    bytes: u64,
    waiting: BTreeSet<usize>,
    /// The bytes of the waiting pieces.
    waiting_bytes: u64,
}

impl PieceStore {
    /// Open the stream file `file` made of `pieces`: resume from the state file
    /// when it describes the same pieces, cutting `.part` back to the bytes it
    /// saved; start over without one. Another piece list is a changed video.
    pub fn open(file: &Path, pieces: &[Piece]) -> Result<PieceStore, Error> {
        let fingerprint = fingerprint(pieces);
        let saved = load_state(&state_path(file));
        if saved.as_ref().is_some_and(|s| s.fingerprint != fingerprint) {
            return Err(Error::FileChanged);
        }
        let mut store = PieceStore {
            file: file.to_path_buf(),
            count: pieces.len(),
            fingerprint,
            written: 0,
            bytes: 0,
            waiting: BTreeSet::new(),
            waiting_bytes: 0,
        };
        let part = part_path(file);
        let resumed = saved.filter(|s| {
            std::fs::metadata(&part).is_ok_and(|m| m.len() >= s.bytes) && s.written <= s.count
        });
        match resumed {
            Some(state) => {
                OpenOptions::new()
                    .write(true)
                    .open(&part)?
                    .set_len(state.bytes)?;
                store.written = state.written;
                store.bytes = state.bytes;
            }
            None => {
                std::fs::write(&part, b"")?;
                let _ = std::fs::remove_dir_all(waiting_dir(file));
            }
        }
        std::fs::create_dir_all(waiting_dir(file))?;
        (store.waiting, store.waiting_bytes) = store.tidy_waiting()?;
        store.save()?;
        Ok(store)
    }

    pub fn written(&self) -> usize {
        self.written
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Every byte kept on disk: the written ones and the waiting pieces'.
    pub fn kept_bytes(&self) -> u64 {
        self.bytes + self.waiting_bytes
    }

    /// Pieces waiting whole in the folder.
    pub fn waiting(&self) -> BTreeSet<usize> {
        self.waiting.clone()
    }

    /// Keep piece `index` whole until the pieces before it are written.
    pub fn keep(&mut self, index: usize, bytes: &[u8]) -> Result<(), Error> {
        let path = self.waiting_file(index);
        let temp = with_suffix(&path, TEMP_SUFFIX);
        let mut out = std::fs::File::create(&temp)?;
        out.write_all(bytes)?;
        out.sync_data()?;
        drop(out);
        std::fs::rename(&temp, &path)?;
        if self.waiting.insert(index) {
            self.waiting_bytes += bytes.len() as u64;
        }
        Ok(())
    }

    /// Append the waiting pieces that come next, in order, to `.part`; the
    /// state is saved once they are on disk. Returns the pieces written.
    pub fn flush(&mut self) -> Result<usize, Error> {
        let mut appended = Vec::new();
        let mut part = None;
        while self.waiting.contains(&self.written) {
            let path = self.waiting_file(self.written);
            let bytes = std::fs::read(&path)?;
            let out = match &mut part {
                Some(out) => out,
                None => part.insert(
                    OpenOptions::new()
                        .append(true)
                        .open(part_path(&self.file))?,
                ),
            };
            out.write_all(&bytes)?;
            self.waiting.remove(&self.written);
            self.waiting_bytes = self.waiting_bytes.saturating_sub(bytes.len() as u64);
            self.written += 1;
            self.bytes += bytes.len() as u64;
            appended.push(path);
        }
        if let Some(out) = part {
            out.sync_data()?;
            self.save()?;
            for path in appended {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(self.written)
    }

    /// Every piece is written: `.part` becomes the stream file, and the state
    /// file and the folder go.
    pub fn finish(self) -> Result<PathBuf, Error> {
        if self.written != self.count {
            return Err(Error::State(format!(
                "{} of {} pieces written",
                self.written, self.count
            )));
        }
        std::fs::rename(part_path(&self.file), &self.file)?;
        let _ = std::fs::remove_file(state_path(&self.file));
        let _ = std::fs::remove_dir_all(waiting_dir(&self.file));
        Ok(self.file)
    }

    fn waiting_file(&self, index: usize) -> PathBuf {
        waiting_dir(&self.file).join(format!("{index:06}"))
    }

    /// The whole pieces in the folder that are still to be written, and their
    /// bytes; half written and already written ones are deleted.
    fn tidy_waiting(&self) -> Result<(BTreeSet<usize>, u64), Error> {
        let mut waiting = BTreeSet::new();
        let mut bytes = 0;
        for entry in std::fs::read_dir(waiting_dir(&self.file))? {
            let entry = entry?;
            let path = entry.path();
            match piece_index(&entry.file_name()) {
                Some(i) if i >= self.written && i < self.count => {
                    waiting.insert(i);
                    bytes += entry.metadata()?.len();
                }
                _ => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok((waiting, bytes))
    }

    fn save(&self) -> Result<(), Error> {
        let state = SavedState {
            version: STATE_VERSION,
            fingerprint: self.fingerprint.clone(),
            count: self.count,
            written: self.written,
            bytes: self.bytes,
        };
        let json = serde_json::to_vec(&state).map_err(|e| Error::State(e.to_string()))?;
        crate::state::write_atomic(&state_path(&self.file), &json)?;
        Ok(())
    }
}

/// The bytes a paused pieces stream keeps for `file`, without touching it:
/// what its state file says is written, and its waiting pieces.
pub fn kept_bytes(file: &Path) -> u64 {
    let Some(state) = load_state(&state_path(file)) else {
        return 0;
    };
    let waiting = std::fs::read_dir(waiting_dir(file)).into_iter().flatten();
    let waiting: u64 = waiting
        .flatten()
        .filter(|entry| {
            piece_index(&entry.file_name()).is_some_and(|i| i >= state.written && i < state.count)
        })
        .filter_map(|entry| entry.metadata().ok().map(|m| m.len()))
        .sum();
    state.bytes + waiting
}

/// The piece a waiting file holds: its name is the index in six digits.
fn piece_index(name: &std::ffi::OsStr) -> Option<usize> {
    name.to_str()
        .filter(|n| n.len() == 6)
        .and_then(|n| n.parse().ok())
}

fn load_state(path: &Path) -> Option<SavedState> {
    let bytes = std::fs::read(path).ok()?;
    let state: SavedState = serde_json::from_slice(&bytes).ok()?;
    (state.version == STATE_VERSION).then_some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pieces::Slice;
    use url::Url;

    fn piece(i: usize, host: &str) -> Piece {
        Piece {
            url: Url::parse(&format!("https://{host}/p/{i}?sig={host}")).unwrap(),
            range: None,
            duration_ms: 4000,
            key: None,
        }
    }

    fn pieces(n: usize) -> Vec<Piece> {
        (0..n).map(|i| piece(i, "a.test")).collect()
    }

    fn body(i: usize) -> Vec<u8> {
        vec![i as u8; 10 + i]
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn names_beside_the_stream_file() {
        let file = Path::new("/d/A.f1.mp4");

        assert_eq!(state_path(file), Path::new("/d/A.f1.mp4.part.pieces"));
        assert_eq!(waiting_dir(file), Path::new("/d/A.f1.mp4.pieces"));
        assert_eq!(
            leftovers(file),
            [
                PathBuf::from("/d/A.f1.mp4.part"),
                PathBuf::from("/d/A.f1.mp4.part.pieces"),
                PathBuf::from("/d/A.f1.mp4.pieces"),
            ]
        );
    }

    #[test]
    fn the_fingerprint_ignores_links_but_not_durations_or_ranges() {
        let list = pieces(4);
        let other_links: Vec<Piece> = (0..4).map(|i| piece(i, "b.test")).collect();
        let mut longer = list.clone();
        longer[2].duration_ms = 4001;
        let mut sliced = list.clone();
        sliced[1].range = Some(Slice { offset: 0, len: 5 });

        assert_eq!(fingerprint(&list), fingerprint(&other_links));
        assert_ne!(fingerprint(&list), fingerprint(&longer));
        assert_ne!(fingerprint(&list), fingerprint(&sliced));
        assert_ne!(fingerprint(&list), fingerprint(&list[..3]));
    }

    #[test]
    fn pieces_reach_the_part_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let mut store = PieceStore::open(&file, &pieces(3)).unwrap();

        store.keep(2, &body(2)).unwrap();
        store.keep(0, &body(0)).unwrap();
        assert_eq!(store.flush().unwrap(), 1);
        assert_eq!(store.written(), 1);
        store.keep(1, &body(1)).unwrap();
        assert_eq!(store.flush().unwrap(), 3);

        let part = std::fs::read(dir.path().join("A.f1.mp4.part")).unwrap();
        assert_eq!(part, [body(0), body(1), body(2)].concat());
        assert_eq!(store.bytes(), part.len() as u64);
        assert!(names(&waiting_dir(&file)).is_empty());
    }

    #[test]
    fn a_resume_keeps_written_and_waiting_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let list = pieces(5);
        let mut store = PieceStore::open(&file, &list).unwrap();
        store.keep(0, &body(0)).unwrap();
        store.keep(2, &body(2)).unwrap();
        store.flush().unwrap();
        drop(store);
        std::fs::write(waiting_dir(&file).join("000003.tmp"), b"half").unwrap();

        let store = PieceStore::open(&file, &list).unwrap();

        assert_eq!(store.written(), 1);
        assert_eq!(store.bytes(), body(0).len() as u64);
        assert_eq!(store.waiting(), BTreeSet::from([2]));
        assert_eq!(names(&waiting_dir(&file)), ["000002"]);
    }

    #[test]
    fn a_part_longer_than_the_state_is_cut_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let list = pieces(3);
        let mut store = PieceStore::open(&file, &list).unwrap();
        store.keep(0, &body(0)).unwrap();
        store.flush().unwrap();
        let saved = store.bytes();
        drop(store);
        // A crash after appending, before the state file was saved.
        let part = dir.path().join("A.f1.mp4.part");
        let mut longer = std::fs::read(&part).unwrap();
        longer.extend_from_slice(&[0xAA; 100]);
        std::fs::write(&part, longer).unwrap();

        let store = PieceStore::open(&file, &list).unwrap();

        assert_eq!(store.written(), 1);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), saved);
    }

    #[test]
    fn another_piece_list_is_a_changed_video() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        drop(PieceStore::open(&file, &pieces(3)).unwrap());

        let reopened = PieceStore::open(&file, &pieces(4));

        assert!(matches!(reopened, Err(Error::FileChanged)), "{reopened:?}");
    }

    #[test]
    fn finish_leaves_only_the_stream_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let mut store = PieceStore::open(&file, &pieces(2)).unwrap();
        store.keep(1, &body(1)).unwrap();
        store.keep(0, &body(0)).unwrap();
        store.flush().unwrap();

        let done = store.finish().unwrap();

        assert_eq!(done, file);
        assert_eq!(std::fs::read(&file).unwrap(), [body(0), body(1)].concat());
        assert_eq!(names(dir.path()), ["A.f1.mp4"]);
    }

    #[test]
    fn finish_before_every_piece_is_written_fails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let store = PieceStore::open(&file, &pieces(2)).unwrap();

        assert!(store.finish().is_err());
        assert!(!file.exists());
    }

    #[test]
    fn the_look_ahead_is_twice_the_connections() {
        assert!(may_start(9, 2, 4));
        assert!(!may_start(10, 2, 4));
        assert!(may_start(0, 0, 1));
        assert!(!may_start(2, 0, 1));
    }
}
