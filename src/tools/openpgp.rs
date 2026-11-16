//! A deliberately narrow OpenPGP reader: it checks one detached signature made
//! with one pinned RSA key, and nothing else. It exists so rustdm can trust
//! yt-dlp's `SHA2-256SUMS` without pulling in a full OpenPGP library.
//!
//! What it accepts (RFC 4880, the format yt-dlp publishes):
//!
//! - the signature file is exactly one binary signature packet (no armor, no
//!   extra packets, no trailing bytes);
//! - version 4, signature class 0x00 (binary document), RSA, hashed with
//!   SHA-256 or SHA-512;
//! - an issuer-fingerprint subpacket, inside the *hashed* area, equal to the
//!   pinned fingerprint; unknown critical subpackets are refused;
//! - the key is one v4 RSA public-key packet whose recomputed fingerprint equals
//!   the pin. The key bytes are compiled into rustdm and never come from the
//!   network.
//!
//! Everything else is an error. The RSA PKCS#1 v1.5 check itself is `ring`'s.

use super::checksum::to_hex;
use ring::digest::{self, SHA1_FOR_LEGACY_USE_ONLY};
use ring::signature::{
    RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA512,
};

const TAG_SIGNATURE: u8 = 2;
const TAG_PUBLIC_KEY: u8 = 6;
const SUBPACKET_CREATION_TIME: u8 = 2;
const SUBPACKET_ISSUER_FINGERPRINT: u8 = 33;
const VERSION_4: u8 = 4;
const CLASS_BINARY_DOCUMENT: u8 = 0x00;
const ALGO_RSA: u8 = 1;
const HASH_SHA256: u8 = 8;
const HASH_SHA512: u8 = 10;
const MIN_RSA_BITS: usize = 2048;
/// Length of a v4 fingerprint (SHA-1), in bytes.
pub const FINGERPRINT_LEN: usize = 20;

pub type Fingerprint = [u8; FINGERPRINT_LEN];

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("malformed OpenPGP data: {0}")]
    Malformed(&'static str),
    #[error("unsupported OpenPGP feature: {0}")]
    Unsupported(&'static str),
    #[error("the signature was not made by the pinned key")]
    WrongSigner,
    #[error("the pinned key does not match its fingerprint")]
    KeyMismatch,
    #[error("the signature does not match the data")]
    BadSignature,
}

/// The one key rustdm trusts, with `n` and `e` as big-endian bytes.
#[derive(Debug, Clone)]
pub struct PinnedKey {
    n: Vec<u8>,
    e: Vec<u8>,
    fingerprint: Fingerprint,
}

/// `AC0C BBE6 ...` or `AC0CBBE6...` (40 hex digits, any case) as 20 bytes.
pub fn parse_fingerprint(text: &str) -> Option<Fingerprint> {
    let hex: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if hex.len() != FINGERPRINT_LEN * 2 {
        return None;
    }
    let mut out = [0u8; FINGERPRINT_LEN];
    for (slot, pair) in out.iter_mut().zip(hex.chunks(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *slot = (high * 16 + low) as u8;
    }
    Some(out)
}

impl PinnedKey {
    /// Read the first packet of `key_bytes` (a binary, "dearmored" public key),
    /// which must be a v4 RSA public key hashing to `expected`. Packets after
    /// the first (user id, self-signatures, subkeys) are not looked at.
    pub fn load(key_bytes: &[u8], expected: &Fingerprint) -> Result<Self, SignatureError> {
        let (tag, body, _rest) = read_packet(key_bytes)?;
        if tag != TAG_PUBLIC_KEY {
            return Err(SignatureError::Malformed(
                "first packet is not a public key",
            ));
        }
        let mut reader = Reader::new(body);
        if reader.u8()? != VERSION_4 {
            return Err(SignatureError::Unsupported("key version"));
        }
        let _created = reader.take(4)?;
        if reader.u8()? != ALGO_RSA {
            return Err(SignatureError::Unsupported("key algorithm"));
        }
        let n = reader.mpi()?;
        let e = reader.mpi()?;
        reader.finish()?;
        if bit_length(n) < MIN_RSA_BITS {
            return Err(SignatureError::Unsupported("RSA key under 2048 bits"));
        }

        // v4 fingerprint: SHA-1 over 0x99, a two-byte length, then the key body.
        let length = u16::try_from(body.len())
            .map_err(|_| SignatureError::Malformed("public key too long"))?;
        let mut context = digest::Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
        context.update(&[0x99]);
        context.update(&length.to_be_bytes());
        context.update(body);
        if context.finish().as_ref() != expected.as_slice() {
            return Err(SignatureError::KeyMismatch);
        }
        Ok(Self {
            n: n.to_vec(),
            e: e.to_vec(),
            fingerprint: *expected,
        })
    }

    pub fn fingerprint_hex(&self) -> String {
        to_hex(&self.fingerprint).to_ascii_uppercase()
    }
}

/// Check that `signature` is a detached signature of `data` made by `key`.
pub fn verify_detached(
    key: &PinnedKey,
    data: &[u8],
    signature: &[u8],
) -> Result<(), SignatureError> {
    let (tag, body, rest) = read_packet(signature)?;
    if tag != TAG_SIGNATURE {
        return Err(SignatureError::Malformed("not a signature packet"));
    }
    if !rest.is_empty() {
        return Err(SignatureError::Malformed("data after the signature packet"));
    }

    let mut reader = Reader::new(body);
    if reader.u8()? != VERSION_4 {
        return Err(SignatureError::Unsupported("signature version"));
    }
    if reader.u8()? != CLASS_BINARY_DOCUMENT {
        return Err(SignatureError::Unsupported("signature class"));
    }
    if reader.u8()? != ALGO_RSA {
        return Err(SignatureError::Unsupported("signature algorithm"));
    }
    let hash_algo = reader.u8()?;
    let rsa_params = match hash_algo {
        HASH_SHA256 => &RSA_PKCS1_2048_8192_SHA256,
        HASH_SHA512 => &RSA_PKCS1_2048_8192_SHA512,
        _ => return Err(SignatureError::Unsupported("hash algorithm")),
    };
    let hashed_len = usize::from(reader.u16()?);
    let hashed_area = reader.take(hashed_len)?;
    check_hashed_area(hashed_area, &key.fingerprint)?;
    // The unhashed area is not covered by the signature, so nothing in it is trusted.
    let unhashed_len = usize::from(reader.u16()?);
    reader.take(unhashed_len)?;
    let digest_start = reader.take(2)?.to_vec();
    let signature_value = reader.mpi()?;
    reader.finish()?;

    // The signed message: the data, then the signature's own header fields
    // (version through the hashed area), then 04 FF and that header's length.
    let header_len = 6 + hashed_len;
    let mut message = Vec::with_capacity(data.len() + header_len + 6);
    message.extend_from_slice(data);
    message.extend_from_slice(&body[..header_len]);
    message.extend_from_slice(&[VERSION_4, 0xFF]);
    message.extend_from_slice(&u32::try_from(header_len).unwrap_or(u32::MAX).to_be_bytes());

    // The two stored hash bytes are a quick check; a mismatch means a damaged file.
    let hash_alg = if hash_algo == HASH_SHA256 {
        &digest::SHA256
    } else {
        &digest::SHA512
    };
    if digest::digest(hash_alg, &message).as_ref()[..2] != digest_start[..] {
        return Err(SignatureError::BadSignature);
    }

    // ring wants the signature as long as the modulus; an MPI drops leading zeros.
    if signature_value.len() > key.n.len() {
        return Err(SignatureError::BadSignature);
    }
    let mut padded = vec![0u8; key.n.len() - signature_value.len()];
    padded.extend_from_slice(signature_value);

    let components = RsaPublicKeyComponents {
        n: key.n.as_slice(),
        e: key.e.as_slice(),
    };
    components
        .verify(rsa_params, &message, &padded)
        .map_err(|_| SignatureError::BadSignature)
}

/// Walk the hashed subpackets: each must be well formed, no unknown critical
/// one may be present, and an issuer fingerprint equal to `pinned` must be there.
fn check_hashed_area(area: &[u8], pinned: &Fingerprint) -> Result<(), SignatureError> {
    let mut reader = Reader::new(area);
    let mut issuer_seen = false;
    while !reader.is_empty() {
        let length = reader.subpacket_length()?;
        if length == 0 {
            return Err(SignatureError::Malformed("empty subpacket"));
        }
        let content = reader.take(length)?;
        let critical = content[0] & 0x80 != 0;
        match content[0] & 0x7f {
            SUBPACKET_ISSUER_FINGERPRINT => {
                if content.len() != 2 + FINGERPRINT_LEN || content[1] != VERSION_4 {
                    return Err(SignatureError::Malformed("issuer fingerprint subpacket"));
                }
                if content[2..] != pinned[..] {
                    return Err(SignatureError::WrongSigner);
                }
                issuer_seen = true;
            }
            SUBPACKET_CREATION_TIME => {}
            _ if critical => return Err(SignatureError::Unsupported("critical subpacket")),
            _ => {}
        }
    }
    if issuer_seen {
        Ok(())
    } else {
        Err(SignatureError::WrongSigner)
    }
}

/// Split one packet off the front of `data`: its tag, body, and what follows.
/// Both header formats are read; indeterminate and partial lengths are refused.
fn read_packet(data: &[u8]) -> Result<(u8, &[u8], &[u8]), SignatureError> {
    let mut reader = Reader::new(data);
    let header = reader.u8()?;
    if header & 0x80 == 0 {
        return Err(SignatureError::Malformed("not a binary OpenPGP packet"));
    }
    let (tag, length) = if header & 0x40 != 0 {
        (header & 0x3f, reader.subpacket_length()?)
    } else {
        let length = match header & 0x03 {
            0 => usize::from(reader.u8()?),
            1 => usize::from(reader.u16()?),
            2 => reader.u32()? as usize,
            _ => return Err(SignatureError::Unsupported("indeterminate packet length")),
        };
        ((header >> 2) & 0x0f, length)
    };
    let body = reader.take(length)?;
    Ok((tag, body, reader.rest()))
}

fn bit_length(big_endian: &[u8]) -> usize {
    match big_endian.first() {
        Some(&first) => big_endian.len() * 8 - first.leading_zeros() as usize,
        None => 0,
    }
}

/// A cursor over a byte slice that fails instead of reading past the end.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn rest(&self) -> &'a [u8] {
        self.data
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], SignatureError> {
        if n > self.data.len() {
            return Err(SignatureError::Malformed("truncated"));
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, SignatureError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SignatureError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, SignatureError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A length as used by subpackets and new-format packets. The partial-length
    /// form (224..=254) is not accepted.
    fn subpacket_length(&mut self) -> Result<usize, SignatureError> {
        let first = self.u8()?;
        match first {
            0..=191 => Ok(usize::from(first)),
            192..=223 => {
                let second = self.u8()?;
                Ok(((usize::from(first) - 192) << 8) + usize::from(second) + 192)
            }
            255 => Ok(self.u32()? as usize),
            _ => Err(SignatureError::Unsupported("partial length")),
        }
    }

    /// A multiprecision integer: a bit count, then that many bits, whose leading
    /// byte is non-zero and whose bit count is exact.
    fn mpi(&mut self) -> Result<&'a [u8], SignatureError> {
        let bits = usize::from(self.u16()?);
        let value = self.take(bits.div_ceil(8))?;
        if value.is_empty() || bit_length(value) != bits {
            return Err(SignatureError::Malformed("integer"));
        }
        Ok(value)
    }

    fn finish(&self) -> Result<(), SignatureError> {
        if self.data.is_empty() {
            Ok(())
        } else {
            Err(SignatureError::Malformed("unexpected trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = include_bytes!("keys/yt-dlp.gpg");
    const SUMS: &[u8] = include_bytes!("../../tests/fixtures/yt-dlp/SHA2-256SUMS");
    const SIG: &[u8] = include_bytes!("../../tests/fixtures/yt-dlp/SHA2-256SUMS.sig");
    const YTDLP_FINGERPRINT: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";

    fn pinned() -> PinnedKey {
        PinnedKey::load(KEY, &parse_fingerprint(YTDLP_FINGERPRINT).unwrap()).unwrap()
    }

    #[test]
    fn the_real_ytdlp_release_verifies() {
        let key = pinned();
        assert_eq!(key.fingerprint_hex(), YTDLP_FINGERPRINT);
        assert_eq!(verify_detached(&key, SUMS, SIG), Ok(()));
    }

    #[test]
    fn a_flipped_byte_in_the_file_fails() {
        let mut data = SUMS.to_vec();
        data[10] ^= 0x01;
        assert_eq!(
            verify_detached(&pinned(), &data, SIG),
            Err(SignatureError::BadSignature)
        );
    }

    /// Bytes 40..50 of the real signature are its unhashed subpackets (the
    /// issuer key id), which the signature does not cover.
    const UNHASHED: std::ops::Range<usize> = 40..50;

    #[test]
    fn a_flipped_byte_anywhere_in_the_signature_fails() {
        let key = pinned();
        for at in (0..SIG.len()).filter(|at| !UNHASHED.contains(at)) {
            let mut sig = SIG.to_vec();
            sig[at] ^= 0x01;
            assert!(
                verify_detached(&key, SUMS, &sig).is_err(),
                "flipping byte {at} was accepted"
            );
        }
    }

    #[test]
    fn a_cut_off_or_extended_signature_fails() {
        let key = pinned();
        for len in 0..SIG.len() {
            assert!(verify_detached(&key, SUMS, &SIG[..len]).is_err());
        }
        let mut longer = SIG.to_vec();
        longer.push(0);
        assert_eq!(
            verify_detached(&key, SUMS, &longer),
            Err(SignatureError::Malformed("data after the signature packet"))
        );
    }

    #[test]
    fn a_key_that_does_not_match_the_pinned_fingerprint_is_refused() {
        let mut wrong = parse_fingerprint(YTDLP_FINGERPRINT).unwrap();
        wrong[19] ^= 1;
        assert_eq!(
            PinnedKey::load(KEY, &wrong).unwrap_err(),
            SignatureError::KeyMismatch
        );
        // The same key with one modulus byte changed no longer hashes to the pin.
        let mut tampered = KEY.to_vec();
        tampered[40] ^= 1;
        assert!(
            PinnedKey::load(&tampered, &parse_fingerprint(YTDLP_FINGERPRINT).unwrap()).is_err()
        );
    }

    #[test]
    fn armored_input_and_the_wrong_packet_type_are_refused() {
        let key = pinned();
        assert!(verify_detached(&key, SUMS, b"-----BEGIN PGP SIGNATURE-----").is_err());
        // A public-key packet where a signature is expected, and the reverse.
        assert!(verify_detached(&key, SUMS, KEY).is_err());
        assert!(PinnedKey::load(SIG, &parse_fingerprint(YTDLP_FINGERPRINT).unwrap()).is_err());
    }

    #[test]
    fn fingerprints_parse_with_or_without_spaces() {
        let spaced = "AC0C BBE6 848D 6A87 3464  AF4E 57CF 6593 3B5A 7581";
        assert_eq!(
            parse_fingerprint(spaced),
            parse_fingerprint(YTDLP_FINGERPRINT)
        );
        assert_eq!(parse_fingerprint("AC0C"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(20)), None);
    }
}
