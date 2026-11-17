//! The OpenPGP reader against signatures built on purpose: one good control,
//! and one for each way a signature must be refused.

use rustdm::testing::tools::{
    issuer_subpacket, sign, sign_sums, sign_with_short_integer, test_fingerprint, test_pinned_key,
    test_public_key_packet, HeaderForm, SigSpec, TEST_CREATION_TIME,
};
use rustdm::tools::openpgp::{
    parse_fingerprint, verify_detached, verify_detached_any, PinnedKey, SignatureError, Verified,
};

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
        Ok(Verified {
            created_unix: u64::from(TEST_CREATION_TIME)
        })
    );
}

#[test]
fn sha256_signatures_verify_too() {
    let spec = SigSpec {
        hash_algo: 8,
        ..SigSpec::default()
    };
    assert!(verify_detached(&test_pinned_key(), DATA, &sign(DATA, &spec)).is_ok());
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

fn verify_spec(spec: &SigSpec) -> Result<Verified, SignatureError> {
    verify_detached(&test_pinned_key(), DATA, &sign(DATA, spec))
}

#[test]
fn an_issuer_fingerprint_only_in_the_unhashed_area_does_not_count() {
    // The unhashed area is not signed, so anyone could put the right name there.
    let spec = SigSpec {
        issuer: None,
        unhashed: issuer_subpacket(&test_fingerprint()),
        ..SigSpec::default()
    };
    assert_eq!(verify_spec(&spec), Err(SignatureError::WrongSigner));
}

#[test]
fn two_issuer_fingerprints_are_refused_even_when_one_is_right() {
    let mut other = test_fingerprint();
    other[3] ^= 0x10;
    for (first, second) in [(test_fingerprint(), other), (other, test_fingerprint())] {
        let spec = SigSpec {
            issuer: Some(first),
            second_issuer: Some(second),
            ..SigSpec::default()
        };
        assert!(verify_spec(&spec).is_err());
    }
    let twice = SigSpec {
        second_issuer: Some(test_fingerprint()),
        ..SigSpec::default()
    };
    assert!(matches!(
        verify_spec(&twice),
        Err(SignatureError::Malformed(_))
    ));
}

#[test]
fn a_known_critical_subpacket_is_fine_next_to_an_unknown_non_critical_one() {
    let spec = SigSpec {
        issuer_critical: true,
        // Type 100, not critical, one byte of content.
        extra_hashed: vec![2, 100, 7],
        ..SigSpec::default()
    };
    assert!(verify_spec(&spec).is_ok());
}

#[test]
fn exactly_one_creation_time_is_required_and_no_expiry_is_allowed() {
    let none = SigSpec {
        creation_time: None,
        ..SigSpec::default()
    };
    assert!(matches!(
        verify_spec(&none),
        Err(SignatureError::Malformed(_))
    ));
    let twice = SigSpec {
        duplicate_creation_time: true,
        ..SigSpec::default()
    };
    assert!(matches!(
        verify_spec(&twice),
        Err(SignatureError::Malformed(_))
    ));
    // Even one that has not run out yet.
    let expiring = SigSpec {
        expiration: Some(u32::MAX),
        ..SigSpec::default()
    };
    assert_eq!(
        verify_spec(&expiring),
        Err(SignatureError::Unsupported("signature expiration time"))
    );
    let when = SigSpec {
        creation_time: Some(1_700_000_000),
        ..SigSpec::default()
    };
    assert_eq!(verify_spec(&when).unwrap().created_unix, 1_700_000_000);
}

#[test]
fn the_new_format_packet_header_is_read_and_a_partial_length_is_not() {
    let new = SigSpec {
        header: HeaderForm::New,
        ..SigSpec::default()
    };
    assert!(verify_spec(&new).is_ok());
    let partial = SigSpec {
        header: HeaderForm::NewPartial,
        ..SigSpec::default()
    };
    assert_eq!(
        verify_spec(&partial),
        Err(SignatureError::Unsupported("partial length"))
    );
}

#[test]
fn a_long_subpacket_length_is_two_octets_not_a_partial_length() {
    // Subpacket lengths 224..=254 in the first octet are ordinary two-octet
    // lengths (RFC 4880 5.2.3.1): this one is 8400 bytes, first octet 224.
    let length = 8400usize;
    let mut subpacket = vec![
        ((length - 192) >> 8) as u8 + 192,
        ((length - 192) & 0xff) as u8,
        100, // an unknown, non-critical type
    ];
    subpacket.extend(std::iter::repeat_n(0u8, length - 1));
    assert_eq!(subpacket[0], 224);
    let spec = SigSpec {
        extra_hashed: subpacket,
        ..SigSpec::default()
    };
    assert!(verify_spec(&spec).is_ok());
}

#[test]
fn bytes_after_the_signature_integer_inside_the_packet_are_refused() {
    let spec = SigSpec {
        trailing_in_body: vec![0],
        ..SigSpec::default()
    };
    assert_eq!(
        verify_spec(&spec),
        Err(SignatureError::Malformed("unexpected trailing bytes"))
    );
}

#[test]
fn a_signature_integer_shorter_than_the_modulus_still_verifies() {
    // An OpenPGP integer drops its leading zeros, so about one signature in 256
    // is a byte short and must be padded back before the RSA check.
    let (data, packet) = sign_with_short_integer(DATA, &SigSpec::default());
    assert!(verify_detached(&test_pinned_key(), &data, &packet).is_ok());
}

#[test]
fn a_successor_key_can_be_pinned_next_to_the_current_one() {
    let sig = sign_sums(DATA);
    let both = [ytdlp_key(), test_pinned_key()];
    assert!(verify_detached_any(&both, DATA, &sig).is_ok());
    // With the other order, and with the test key alone, the same.
    let swapped = [test_pinned_key(), ytdlp_key()];
    assert!(verify_detached_any(&swapped, DATA, &sig).is_ok());
    // But a key that is not in the list does not sign for it.
    assert_eq!(
        verify_detached_any(&[ytdlp_key()], DATA, &sig),
        Err(SignatureError::WrongSigner)
    );
    assert_eq!(
        verify_detached_any(&[], DATA, &sig),
        Err(SignatureError::WrongSigner)
    );
}
