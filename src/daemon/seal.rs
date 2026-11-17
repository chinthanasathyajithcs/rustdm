//! Seal bytes so only the current user can read them back.
//!
//! Windows: DPAPI (`CryptProtectData`), tied to the signed-in user, with
//! `entropy` as an extra secret the caller must repeat to unseal. Elsewhere
//! (development only) there is no sealing: the bytes pass through, and the
//! caller relies on file permissions.

use std::io;

/// Which scheme `seal` uses here, stored beside the sealed bytes so a file
/// from another platform is recognised instead of misread.
#[cfg(windows)]
pub const SCHEME: u8 = 1;
#[cfg(not(windows))]
pub const SCHEME: u8 = 2;

/// Seal `plain`. `entropy` must be given again to `unseal`.
pub fn seal(plain: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    imp::seal(plain, entropy)
}

/// Undo `seal`. Fails for another user, another `entropy`, or damaged bytes.
pub fn unseal(sealed: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    imp::unseal(sealed, entropy)
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn seal(plain: &[u8], _entropy: &[u8]) -> io::Result<Vec<u8>> {
        Ok(plain.to_vec())
    }

    pub fn unseal(sealed: &[u8], _entropy: &[u8]) -> io::Result<Vec<u8>> {
        Ok(sealed.to_vec())
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    /// `DATA_BLOB`: a length and a pointer.
    #[repr(C)]
    struct DataBlob {
        len: u32,
        data: *mut u8,
    }

    /// Never show a prompt, whatever the system policy says.
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            optional_entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description_out: *mut *mut u16,
            optional_entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// A buffer DPAPI allocated, freed with `LocalFree` when dropped.
    struct Allocated(DataBlob);

    impl Allocated {
        fn empty() -> Self {
            Self(DataBlob {
                len: 0,
                data: ptr::null_mut(),
            })
        }

        fn to_vec(&self) -> Vec<u8> {
            if self.0.data.is_null() {
                return Vec::new();
            }
            // SAFETY: on success DPAPI returns `len` readable bytes at `data`.
            unsafe { std::slice::from_raw_parts(self.0.data, self.0.len as usize) }.to_vec()
        }
    }

    impl Drop for Allocated {
        fn drop(&mut self) {
            if !self.0.data.is_null() {
                // SAFETY: the buffer came from DPAPI, which documents LocalFree for it.
                unsafe { LocalFree(self.0.data.cast()) };
            }
        }
    }

    /// A view of `bytes` for a call that only reads it. The pointer is cast to
    /// mutable because the struct has one type for input and output; DPAPI
    /// does not write through an input blob.
    fn blob_of(bytes: &[u8]) -> io::Result<DataBlob> {
        let len = u32::try_from(bytes.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "data is too large to seal")
        })?;
        Ok(DataBlob {
            len,
            data: bytes.as_ptr().cast_mut(),
        })
    }

    pub fn seal(plain: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob_of(plain)?;
        let extra = blob_of(entropy)?;
        let mut output = Allocated::empty();
        // SAFETY: `input` and `extra` point at slices that outlive the call;
        // `output` receives a buffer that `Allocated` frees.
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &extra,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(output.to_vec())
    }

    pub fn unseal(sealed: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob_of(sealed)?;
        let extra = blob_of(entropy)?;
        let mut output = Allocated::empty();
        // SAFETY: as in `seal`. The description out-parameter is left null, so
        // nothing else is allocated.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &extra,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(output.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_bytes_unseal_to_the_original() {
        let sealed = seal(b"sid=abc123", b"rustdm-cookies:dl_1").unwrap();
        let plain = unseal(&sealed, b"rustdm-cookies:dl_1").unwrap();
        assert_eq!(plain, b"sid=abc123");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_hides_the_plain_bytes() {
        let sealed = seal(b"sid=abc123-very-recognisable", b"e").unwrap();
        assert!(!sealed
            .windows(b"recognisable".len())
            .any(|w| w == b"recognisable"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_refuses_the_wrong_entropy() {
        let sealed = seal(b"sid=abc123", b"rustdm-cookies:dl_1").unwrap();
        assert!(unseal(&sealed, b"rustdm-cookies:dl_2").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_refuses_a_tampered_blob() {
        let mut sealed = seal(b"sid=abc123", b"e").unwrap();
        let middle = sealed.len() / 2;
        sealed[middle] ^= 0xFF;
        assert!(unseal(&sealed, b"e").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_refuses_garbage() {
        assert!(unseal(b"not a dpapi blob", b"e").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn without_dpapi_the_scheme_is_plain() {
        assert_eq!(SCHEME, 2);
    }
}
