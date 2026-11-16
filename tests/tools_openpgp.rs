//! The OpenPGP reader against signatures built on purpose: one good control,
//! and one for each way a signature must be refused.

use rustdm::testing::tools::{
    sign, sign_sums, test_fingerprint, test_pinned_key, test_public_key_packet, SigSpec,
};
use rustdm::tools::openpgp::{parse_fingerprint, verify_detached, PinnedKey, SignatureError};

const DATA: &[u8] = b"0123  yt-dlp.exe\n";
const YTDLP_KEY: &[u8] = include_bytes!("../src/tools/keys/yt-dlp.gpg");
const YTDLP_FINGERPRINT: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";

fn ytdlp_key() -> PinnedKey {
    PinnedKey::load(YTDLP_KEY, &parse_fingerprint(YTDLP_FINGERPRINT).unwrap()).unwrap()
}

#[test]
fn a_good_signature_by_the_test_key_verifies_against_the_test_key() {
    assert_eq!(
        verify_detached(&test_pinned_key(), DATA, &sign_sums(DATA)),
        Ok(())
    );
}

#[test]
fn sha256_signatures_verify_too() {
    let spec = SigSpec {
        hash_algo: 8,
        ..SigSpec::default()
    };
    assert_eq!(
        verify_detached(&test_pinned_key(), DATA, &sign(DATA, &spec)),
        Ok(())
    );
}

#[test]
fn a_test_key_signature_is_refused_by_the_ytdlp_key() {
    let err = verify_detached(&ytdlp_key(), DATA, &sign_sums(DATA)).unwrap_err();
    assert_eq!(err, SignatureError::WrongSigner);
}

#[test]
fn claiming_to_be_the_ytdlp_key_does_not_help() {
    // Signed by the test key but naming yt-dlp's fingerprint as the issuer.
    let spec = SigSpec {
        issuer: parse_fingerprint(YTDLP_FINGERPRINT),
        ..SigSpec::default()
    };
    let err = verify_detached(&ytdlp_key(), DATA, &sign(DATA, &spec)).unwrap_err();
    assert_eq!(err, SignatureError::BadSignature);
}

#[test]
fn a_changed_file_fails() {
    let sig = sign_sums(DATA);
    let err = verify_detached(&test_pinned_key(), b"other", &sig).unwrap_err();
    assert_eq!(err, SignatureError::BadSignature);
}

#[test]
fn each_unsupported_kind_of_signature_is_refused() {
    let key = test_pinned_key();
    let cases = [
        (
            "class 0x01",
            SigSpec {
                class: 0x01,
                ..SigSpec::default()
            },
        ),
        (
            "version 3",
            SigSpec {
                version: 3,
                ..SigSpec::default()
            },
        ),
        (
            "DSA",
            SigSpec {
                pk_algo: 17,
                ..SigSpec::default()
            },
        ),
        (
            "EdDSA",
            SigSpec {
                pk_algo: 22,
                ..SigSpec::default()
            },
        ),
        (
            "SHA-1",
            SigSpec {
                hash_algo: 2,
                ..SigSpec::default()
            },
        ),
        (
            "SHA-384",
            SigSpec {
                hash_algo: 9,
                ..SigSpec::default()
            },
        ),
        (
            "unknown critical subpacket",
            SigSpec {
                unknown_critical: true,
                ..SigSpec::default()
            },
        ),
    ];
    for (name, spec) in cases {
        let result = verify_detached(&key, DATA, &sign(DATA, &spec));
        assert!(
            matches!(result, Err(SignatureError::Unsupported(_))),
            "{name} gave {result:?}"
        );
    }
}

#[test]
fn a_signature_without_an_issuer_fingerprint_is_refused() {
    let spec = SigSpec {
        issuer: None,
        ..SigSpec::default()
    };
    assert_eq!(
        verify_detached(&test_pinned_key(), DATA, &sign(DATA, &spec)),
        Err(SignatureError::WrongSigner)
    );
}

#[test]
fn the_pinned_key_must_hash_to_its_fingerprint() {
    let packet = test_public_key_packet();
    assert!(PinnedKey::load(&packet, &test_fingerprint()).is_ok());
    let mut wrong = test_fingerprint();
    wrong[0] ^= 0xff;
    assert_eq!(
        PinnedKey::load(&packet, &wrong).unwrap_err(),
        SignatureError::KeyMismatch
    );
}
