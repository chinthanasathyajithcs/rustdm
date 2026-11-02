use crate::error::Error;
use std::io;
use std::path::Path;

pub const SAFETY_MARGIN_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB safety margin

/// Returns the existing ancestor path if `path` does not exist yet.
fn find_existing_ancestor(path: &Path) -> io::Result<&Path> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no existing ancestor found for path: {}", path.display()),
                ));
            }
        }
    }
}

#[cfg(unix)]
fn os_available_space(existing_path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(existing_path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if res != 0 {
        return Err(io::Error::last_os_error());
    }

    let bavail = stat.f_bavail as u64;
    let frsize = stat.f_frsize as u64;
    Ok(bavail.saturating_mul(frsize))
}

#[cfg(windows)]
fn os_available_space(existing_path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = existing_path.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut free_bytes_available: u64 = 0;
    let mut total_number_of_bytes: u64 = 0;
    let mut total_number_of_free_bytes: u64 = 0;

    let ret = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes_available,
            &mut total_number_of_bytes,
            &mut total_number_of_free_bytes,
        )
    };

    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(free_bytes_available)
}

#[cfg(not(any(unix, windows)))]
fn os_available_space(_existing_path: &Path) -> io::Result<u64> {
    compile_error!("unsupported target platform");
}

/// Returns the free bytes available to an unprivileged caller at the target path or its nearest existing ancestor.
pub fn available_space(path: &Path) -> io::Result<u64> {
    let existing = find_existing_ancestor(path)?;
    os_available_space(existing)
}

/// Verifies that the filesystem containing `target_path` has enough free space for `required_bytes` plus `safety_margin_bytes`.
pub fn ensure_disk_space(
    target_path: &Path,
    required_bytes: u64,
    safety_margin_bytes: u64,
) -> Result<(), Error> {
    let available = available_space(target_path)?;
    let total_needed = required_bytes.saturating_add(safety_margin_bytes);
    if available < total_needed {
        return Err(Error::InsufficientDiskSpace {
            target: target_path.display().to_string(),
            required: total_needed,
            available,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn available_space_reports_non_zero_for_tempdir() {
        let dir = tempdir().unwrap();
        let space = available_space(dir.path()).unwrap();
        assert!(space > 0, "available space should be greater than zero");
    }

    #[test]
    fn available_space_walks_up_for_non_existent_file() {
        let dir = tempdir().unwrap();
        let non_existent = dir.path().join("sub1").join("sub2").join("file.part");
        let space = available_space(&non_existent).unwrap();
        assert!(
            space > 0,
            "should walk up ancestors to find available space"
        );
    }

    #[test]
    fn ensure_disk_space_succeeds_for_small_request() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("test.bin");
        let res = ensure_disk_space(&target, 1024, 0);
        assert!(res.is_ok());
    }

    #[test]
    fn ensure_disk_space_fails_when_impossibly_large() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("test.bin");
        let res = ensure_disk_space(&target, u64::MAX / 2, 0);
        match res {
            Err(Error::InsufficientDiskSpace {
                target: t,
                required,
                available,
            }) => {
                assert_eq!(t, target.display().to_string());
                assert_eq!(required, u64::MAX / 2);
                assert!(available < required);
            }
            other => panic!("expected InsufficientDiskSpace, got {other:?}"),
        }
    }
}
