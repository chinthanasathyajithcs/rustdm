//! Helpers for testing the tools manager without touching github.com: a
//! committed, test-only RSA key that signs checksum files the way yt-dlp's
//! release key does, and builders for deliberately malformed signatures.
//!
//! The key in `test_signing_key.der` signs nothing real and must never be
//! used for anything else.

use crate::tools::openpgp::{Fingerprint, PinnedKey};
use ring::digest::{self, SHA1_FOR_LEGACY_USE_ONLY};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256, RSA_PKCS1_SHA512};

const TEST_KEY_DER: &[u8] = include_bytes!("test_signing_key.der");

/// How to build a signature. The default is a good one: v4, class 0x00, RSA,
/// SHA-512, issued by the test key. Change a field to get a signature the
/// reader must refuse.
#[derive(Clone, Debug)]
pub struct SigSpec {
    pub version: u8,
    pub class: u8,
    pub pk_algo: u8,
    pub hash_algo: u8,
    /// Fingerprint named in the hashed issuer subpacket; `None` leaves it out.
    pub issuer: Option<Fingerprint>,
    /// Set the critical bit on the issuer subpacket (a known type, so it is fine).
    pub issuer_critical: bool,
    /// A second issuer subpacket in the hashed area.
    pub second_issuer: Option<Fingerprint>,
    /// Signature creation time in the hashed area; `None` leaves it out.
    pub creation_time: Option<u32>,
    /// Put the creation time in the hashed area twice.
    pub duplicate_creation_time: bool,
    /// A signature-expiration-time subpacket (seconds after creation).
    pub expiration: Option<u32>,
    /// Add a critical subpacket of an unknown type to the hashed area.
    pub unknown_critical: bool,
    /// Raw subpacket bytes appended to the hashed area as they are.
    pub extra_hashed: Vec<u8>,
    /// The raw bytes of the unhashed area, which the signature does not cover.
    pub unhashed: Vec<u8>,
    /// Bytes placed after the signature integer, inside the packet body.
    pub trailing_in_body: Vec<u8>,
    /// How the packet header is written.
    pub header: HeaderForm,
}

/// The ways a packet header can be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderForm {
    /// `0x89` and a two-octet length, what gpg writes.
    Old,
    /// `0xC2` and a new-format length.
    New,
    /// `0xC2` with a partial body length, which must be refused.
    NewPartial,
}

/// The default creation time of a test signature.
pub const TEST_CREATION_TIME: u32 = 0x6adf_d329;

/// A hashed or unhashed issuer-fingerprint subpacket naming `fingerprint`.
pub fn issuer_subpacket(fingerprint: &Fingerprint) -> Vec<u8> {
    let mut out = vec![22, 33, 4];
    out.extend(fingerprint);
    out
}

impl Default for SigSpec {
    fn default() -> Self {
        Self {
            version: 4,
            class: 0x00,
            pk_algo: 1,
            hash_algo: 10,
            issuer: Some(test_fingerprint()),
            issuer_critical: false,
            second_issuer: None,
            creation_time: Some(TEST_CREATION_TIME),
            duplicate_creation_time: false,
            expiration: None,
            unknown_critical: false,
            extra_hashed: Vec::new(),
            unhashed: Vec::new(),
            trailing_in_body: Vec::new(),
            header: HeaderForm::Old,
        }
    }
}

fn key_pair() -> RsaKeyPair {
    RsaKeyPair::from_der(TEST_KEY_DER).expect("committed test key parses")
}

/// An OpenPGP multiprecision integer: bit count, then the bytes.
fn mpi(big_endian: &[u8]) -> Vec<u8> {
    let first = big_endian.iter().position(|&b| b != 0).unwrap_or(0);
    let value = &big_endian[first..];
    let bits = value.len() * 8 - value.first().map_or(0, |b| b.leading_zeros() as usize);
    let mut out = (bits as u16).to_be_bytes().to_vec();
    out.extend_from_slice(value);
    out
}

/// Read one DER element: its tag must be `tag`; returns the content and what
/// follows. Lengths up to two bytes are enough for a 2048-bit key.
fn der_element(data: &[u8], tag: u8) -> (&[u8], &[u8]) {
    assert_eq!(data[0], tag, "unexpected DER tag");
    let (len, header) = match data[1] {
        n if n < 0x80 => (usize::from(n), 2),
        0x81 => (usize::from(data[2]), 3),
        0x82 => (usize::from(u16::from_be_bytes([data[2], data[3]])), 4),
        _ => panic!("unexpected DER length"),
    };
    (&data[header..header + len], &data[header + len..])
}

/// The body of the test key's v4 public-key packet.
fn public_key_body() -> Vec<u8> {
    let key = key_pair();
    // ring hands out the public key as DER: SEQUENCE { INTEGER n, INTEGER e }.
    let (sequence, _) = der_element(key.public().as_ref(), 0x30);
    let (n, rest) = der_element(sequence, 0x02);
    let (e, _) = der_element(rest, 0x02);
    let mut body = vec![4, 0, 0, 0, 0, 1];
    body.extend(mpi(n));
    body.extend(mpi(e));
    body
}

/// The test key as a binary OpenPGP public-key packet, like a dearmored `public.key`.
pub fn test_public_key_packet() -> Vec<u8> {
    let body = public_key_body();
    let mut packet = vec![0x99];
    packet.extend((body.len() as u16).to_be_bytes());
    packet.extend(body);
    packet
}

pub fn test_fingerprint() -> Fingerprint {
    let body = public_key_body();
    let mut context = digest::Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    context.update(&[0x99]);
    context.update(&(body.len() as u16).to_be_bytes());
    context.update(&body);
    let mut out = [0u8; 20];
    out.copy_from_slice(context.finish().as_ref());
    out
}

/// The test key as a `PinnedKey`, for a store that trusts it instead of yt-dlp's.
pub fn test_pinned_key() -> PinnedKey {
    PinnedKey::load(&test_public_key_packet(), &test_fingerprint()).expect("test key loads")
}

/// A detached signature of `data` made by the test key, as a binary packet.
pub fn sign(data: &[u8], spec: &SigSpec) -> Vec<u8> {
    build(data, spec).0
}

/// Like `sign`, also saying whether the signature integer lost a leading zero
/// byte (an OpenPGP integer has none, so it is shorter than the modulus).
fn build(data: &[u8], spec: &SigSpec) -> (Vec<u8>, bool) {
    let mut hashed = Vec::new();
    if let Some(issuer) = &spec.issuer {
        let mut subpacket = issuer_subpacket(issuer);
        if spec.issuer_critical {
            subpacket[1] |= 0x80;
        }
        hashed.extend(subpacket);
    }
    if let Some(issuer) = &spec.second_issuer {
        hashed.extend(issuer_subpacket(issuer));
    }
    if let Some(created) = spec.creation_time {
        let times = if spec.duplicate_creation_time { 2 } else { 1 };
        for _ in 0..times {
            hashed.extend([5, 2]);
            hashed.extend(created.to_be_bytes());
        }
    }
    if let Some(seconds) = spec.expiration {
        hashed.extend([5, 3]);
        hashed.extend(seconds.to_be_bytes());
    }
    if spec.unknown_critical {
        hashed.extend([2, 0x80 | 100, 0]);
    }
    hashed.extend(&spec.extra_hashed);

    let mut header = vec![spec.version, spec.class, spec.pk_algo, spec.hash_algo];
    header.extend((hashed.len() as u16).to_be_bytes());
    header.extend(&hashed);

    let mut message = data.to_vec();
    message.extend(&header);
    message.extend([4, 0xFF]);
    message.extend((header.len() as u32).to_be_bytes());

    let (digest_alg, sign_alg) = if spec.hash_algo == 8 {
        (
            &digest::SHA256,
            &RSA_PKCS1_SHA256 as &'static dyn ring::signature::RsaEncoding,
        )
    } else {
        (
            &digest::SHA512,
            &RSA_PKCS1_SHA512 as &'static dyn ring::signature::RsaEncoding,
        )
    };
    let left16 = digest::digest(digest_alg, &message).as_ref()[..2].to_vec();
    let key = key_pair();
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(sign_alg, &SystemRandom::new(), &message, &mut signature)
        .expect("sign with the test key");
    let short = signature[0] == 0;

    let mut body = header;
    body.extend((spec.unhashed.len() as u16).to_be_bytes());
    body.extend(&spec.unhashed);
    body.extend(left16);
    body.extend(mpi(&signature));
    body.extend(&spec.trailing_in_body);
    (packet(&body, spec.header), short)
}

/// `body` as a signature packet (tag 2) with the chosen header.
fn packet(body: &[u8], form: HeaderForm) -> Vec<u8> {
    let mut packet = match form {
        HeaderForm::Old => {
            let mut head = vec![0x89];
            head.extend((body.len() as u16).to_be_bytes());
            head
        }
        HeaderForm::New => {
            // Two-octet new-format length: 192 + ((first - 192) << 8) + second.
            let n = body.len() - 192;
            vec![0xC2, (n >> 8) as u8 + 192, n as u8]
        }
        HeaderForm::NewPartial => vec![0xC2, 0xE9],
    };
    packet.extend(body);
    packet
}

/// A signature whose integer is shorter than the modulus, with the data it
/// signs. Signs `prefix` plus a counter until one comes out short (about one
/// in 256 does).
pub fn sign_with_short_integer(prefix: &[u8], spec: &SigSpec) -> (Vec<u8>, Vec<u8>) {
    for counter in 0u32..100_000 {
        let mut data = prefix.to_vec();
        data.extend(counter.to_be_bytes());
        let (packet, short) = build(&data, spec);
        if short {
            return (data, packet);
        }
    }
    panic!("no short signature in 100000 tries");
}

/// A good signature of `data` by the test key, as `SHA2-256SUMS.sig` would be.
pub fn sign_sums(data: &[u8]) -> Vec<u8> {
    sign(data, &SigSpec::default())
}
