//! AES-128 pieces (`EXT-X-KEY:METHOD=AES-128`): CBC with PKCS#7 padding
//! (spec 1c, section 7.6). Keys live in memory only and are never logged.

use crate::error::Error;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

/// The plain bytes of a piece sealed with `key` and `iv`.
pub fn decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, Error> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Error::Protocol("an encrypted piece could not be decrypted".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};

    fn encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(plain)
    }

    #[test]
    fn a_piece_encrypted_with_a_known_key_decrypts() {
        let plain = b"pieces mode test".repeat(5);
        let sealed = encrypt(&[7; 16], &[9; 16], &plain);

        assert_ne!(sealed, plain);
        assert_eq!(decrypt(&[7; 16], &[9; 16], &sealed).unwrap(), plain);
    }

    #[test]
    fn a_wrong_key_or_a_cut_piece_is_an_error() {
        let sealed = encrypt(&[7; 16], &[9; 16], b"pieces mode test, uneven");

        let wrong = decrypt(&[8; 16], &[9; 16], &sealed);
        let cut = decrypt(&[7; 16], &[9; 16], &sealed[..sealed.len() - 3]);

        assert!(matches!(wrong, Err(Error::Protocol(_))), "{wrong:?}");
        assert!(matches!(cut, Err(Error::Protocol(_))), "{cut:?}");
    }
}
