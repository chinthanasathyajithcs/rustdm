//! Native messaging frames: a 4-byte native-endian length, then that many bytes of JSON.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame either side accepts (Chrome's own limit for host to browser is 1 MiB).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes is over the {MAX_FRAME_BYTES} byte limit")]
    TooLarge(u64),
    #[error("input ended in the middle of a frame")]
    Truncated,
}

/// Read one frame. `Ok(None)` means the input ended cleanly between frames.
/// The length is checked before anything is allocated for the body.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut prefix = [0u8; 4];
    let mut filled = 0;
    while filled < prefix.len() {
        let n = reader.read(&mut prefix[filled..]).await?;
        if n == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        filled += n;
    }
    let len = u32::from_ne_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len as u64));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(Some(body))
}

/// Write one frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), FrameError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len() as u64));
    }
    writer.write_all(&(body.len() as u32).to_ne_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(bytes: &[u8]) -> Result<Option<Vec<u8>>, FrameError> {
        read_frame(&mut &bytes[..]).await
    }

    #[tokio::test]
    async fn a_frame_round_trips() {
        let mut wire = Vec::new();
        write_frame(&mut wire, br#"{"type":"ping","id":1}"#)
            .await
            .unwrap();
        let mut input = &wire[..];
        assert_eq!(
            read_frame(&mut input).await.unwrap().unwrap(),
            br#"{"type":"ping","id":1}"#
        );
        assert!(read_frame(&mut input).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_length_prefix_is_native_endian() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"abc").await.unwrap();
        assert_eq!(&wire[..4], &3u32.to_ne_bytes());
        assert_eq!(&wire[4..], b"abc");
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_the_body_is_read() {
        let wire = ((MAX_FRAME_BYTES + 1) as u32).to_ne_bytes();
        // No body follows: reading it would report Truncated, not TooLarge.
        let err = read_all(&wire).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(n) if n == MAX_FRAME_BYTES as u64 + 1));
    }

    #[tokio::test]
    async fn the_largest_frame_is_accepted() {
        let body = vec![b' '; MAX_FRAME_BYTES];
        let mut wire = Vec::new();
        write_frame(&mut wire, &body).await.unwrap();
        assert_eq!(
            read_all(&wire).await.unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );
    }

    #[tokio::test]
    async fn writing_an_oversized_frame_fails_without_output() {
        let mut wire = Vec::new();
        let err = write_frame(&mut wire, &vec![0u8; MAX_FRAME_BYTES + 1])
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn a_cut_off_prefix_or_body_is_an_error() {
        assert!(matches!(
            read_all(&[1, 0]).await.unwrap_err(),
            FrameError::Truncated
        ));
        let mut wire = 5u32.to_ne_bytes().to_vec();
        wire.extend_from_slice(b"ab");
        assert!(matches!(
            read_all(&wire).await.unwrap_err(),
            FrameError::Truncated
        ));
    }

    #[tokio::test]
    async fn empty_input_is_a_clean_end() {
        assert!(read_all(&[]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn line_feed_and_carriage_return_bytes_in_the_prefix_survive() {
        // Lengths 10 and 13 put LF and CR in the prefix; 266 has 0x0A in the second byte.
        for len in [10usize, 13, 266] {
            let body = vec![b'x'; len];
            let mut wire = Vec::new();
            write_frame(&mut wire, &body).await.unwrap();
            assert_eq!(read_all(&wire).await.unwrap().unwrap(), body);
        }
    }
}
