//! The little bit of the Windows registry that browser registration needs,
//! under `HKEY_CURRENT_USER`, by raw calls into advapi32 (the style of
//! `src/disk.rs`).

use std::ffi::c_void;
use std::io;

type Hkey = isize;
const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize;
const REG_SZ: u32 = 1;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(
        hkey: Hkey,
        sub_key: *const u16,
        value_name: *const u16,
        value_type: u32,
        data: *const c_void,
        data_len: u32,
    ) -> u32;
    fn RegDeleteKeyW(hkey: Hkey, sub_key: *const u16) -> u32;
    fn RegGetValueW(
        hkey: Hkey,
        sub_key: *const u16,
        value_name: *const u16,
        flags: u32,
        value_type: *mut u32,
        data: *mut c_void,
        data_len: *mut u32,
    ) -> u32;
}

/// `text` as a NUL-terminated UTF-16 string.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Set the default value of `HKCU\<sub_key>` to `value`, creating the key.
pub fn set_default(sub_key: &str, value: &str) -> io::Result<()> {
    let key = wide(sub_key);
    let data = wide(value);
    let bytes = (data.len() * 2) as u32;
    // SAFETY: `key` and `data` are NUL-terminated and outlive the call; a null
    // value name means the key's default value; `bytes` is the size of `data`.
    check(unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            std::ptr::null(),
            REG_SZ,
            data.as_ptr().cast(),
            bytes,
        )
    })
}

/// The default value of `HKCU\<sub_key>`, or `None` when the key or value is missing.
pub fn get_default(sub_key: &str) -> io::Result<Option<String>> {
    let key = wide(sub_key);
    let mut bytes = 0u32;
    // SAFETY: a null data pointer only asks for the size; `key` is NUL-terminated.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;

    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut capacity = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `capacity` bytes, which is what is passed.
    check(unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut capacity,
        )
    })?;
    let units = (capacity as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..units]);
    Ok(Some(text.trim_end_matches('\0').to_string()))
}

/// Delete `HKCU\<sub_key>` (which must have no sub-keys). Returns false when it did not exist.
pub fn delete_key(sub_key: &str) -> io::Result<bool> {
    let key = wide(sub_key);
    // SAFETY: `key` is NUL-terminated and outlives the call.
    let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key.as_ptr()) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(code)?;
    Ok(true)
}
