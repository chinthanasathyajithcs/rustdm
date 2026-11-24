//! Pieces mode: a stream sent as many small pieces (HLS, DASH) is downloaded
//! several pieces at a time and written in order into one stream file
//! (spec 1c, section 7).

use url::Url;

/// The most pieces one stream may have.
pub const MAX_PIECES: usize = 100_000;

/// One piece of a stream, in the order the stream is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub url: Url,
    /// Only these bytes of `url`, when the piece is a slice of a bigger file.
    pub range: Option<Slice>,
    /// How long the piece plays, in milliseconds; 0 when not given.
    pub duration_ms: u64,
    /// Decrypt the piece with this key, when it is encrypted.
    pub key: Option<Key>,
}

/// A slice of a file: `len` bytes from `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    pub offset: u64,
    pub len: u64,
}

/// An AES-128 key: where to fetch it, and the IV of the piece it applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub url: Url,
    pub iv: [u8; 16],
}

mod store;
pub use store::{fingerprint, leftovers, may_start, state_path, waiting_dir, PieceStore};
