//! Make sure `rdm` only talks to a daemon run by the same user. Another user
//! on the machine could otherwise create the socket or pipe first and receive
//! every URL (and any token in it) that `rdm` sends.

use std::io;

fn not_ours(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{what} belongs to another user; refusing to connect"),
    )
}

#[cfg(unix)]
mod unix {
    use super::not_ours;
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    /// The current user's id.
    pub fn current_uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Fail unless the socket at `path` is owned by `uid`. A missing socket
    /// passes, so connecting reports "not running" as usual.
    pub fn check_socket_owner(path: &Path, uid: u32) -> io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.uid() == uid => Ok(()),
            Ok(_) => Err(not_ours(&format!("socket {}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Let only the owner connect, whatever the umask was.
    pub fn restrict_socket(path: &Path) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
}

#[cfg(unix)]
pub use unix::{check_socket_owner, current_uid, restrict_socket};

#[cfg(windows)]
mod windows {
    use super::not_ours;
    use std::ffi::c_void;
    use std::io;

    type Handle = *mut c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetNamedPipeServerProcessId(pipe: Handle, process_id: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            info: *mut c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn EqualSid(a: *mut c_void, b: *mut c_void) -> i32;
    }

    /// Closes a kernel handle when dropped.
    struct Owned(Handle);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess or OpenProcessToken and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The TOKEN_USER of `process`, in a buffer aligned for the pointer it starts with.
    fn token_user(process: Handle) -> io::Result<Vec<u64>> {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: `token` is a valid out-pointer.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Owned(token);

        let mut needed = 0u32;
        // SAFETY: a null buffer with length 0 only asks for the needed size.
        unsafe {
            GetTokenInformation(
                token.0,
                TOKEN_USER_CLASS,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `needed` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    /// The SID pointer at the start of a TOKEN_USER buffer.
    fn sid(token_user: &[u64]) -> *mut c_void {
        token_user[0] as usize as *mut c_void
    }

    /// Fail unless the process serving `pipe` runs as the current user.
    pub fn check_pipe_server_owner(
        pipe: &impl std::os::windows::io::AsRawHandle,
        name: &str,
    ) -> io::Result<()> {
        let mut pid = 0u32;
        // SAFETY: `pipe` owns an open pipe handle for this call, and `pid` is a
        // valid out-pointer.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcess has no memory preconditions.
        let server = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if server.is_null() {
            return Err(not_ours(&format!("pipe {name} (process {pid})")));
        }
        let server = Owned(server);

        let theirs = token_user(server.0).map_err(|_| not_ours(&format!("pipe {name}")))?;
        // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing.
        let ours = token_user(unsafe { GetCurrentProcess() })?;
        // SAFETY: both pointers point at SIDs inside live TOKEN_USER buffers.
        if unsafe { EqualSid(sid(&theirs), sid(&ours)) } == 0 {
            return Err(not_ours(&format!("pipe {name}")));
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use windows::check_pipe_server_owner;

/// A name part made only of letters, digits, `-` and `_`, for per-user
/// endpoint names. Anything else becomes `_`.
pub fn endpoint_safe(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_names_keep_only_safe_characters() {
        assert_eq!(endpoint_safe("Chinthana S."), "Chinthana_S_");
        assert_eq!(endpoint_safe("dev-01_x"), "dev-01_x");
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_owned_by_someone_else_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sock");
        std::fs::write(&path, b"").unwrap();

        check_socket_owner(&path, current_uid()).unwrap();
        let err = check_socket_owner(&path, current_uid() + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("another user"), "{err}");
        // Nothing there yet: let the connection attempt report it.
        check_socket_owner(&dir.path().join("missing.sock"), 1).unwrap();
    }
}
