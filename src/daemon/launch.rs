use crate::ipc::transport::IpcEndpoint;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
    fn SetHandleInformation(h_object: *mut std::ffi::c_void, dw_mask: u32, dw_flags: u32) -> i32;
}

/// Windows passes every inheritable handle to a child process, even when its
/// own stdio is redirected. If this process's stdio is a pipe (output captured
/// by a script or a test), the long-lived daemon would hold that pipe open, and
/// whoever reads it would wait forever for EOF. Mark this process's stdio as
/// not inheritable before starting the daemon.
#[cfg(windows)]
fn stop_stdio_inheritance() {
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: isize = -1;

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle has no preconditions; SetHandleInformation only
        // runs on a handle GetStdHandle returned as valid.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle as isize != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Path of the `rdmd` executable installed next to the running binary.
pub fn daemon_exe_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable has no parent directory",
        )
    })?;
    Ok(dir.join(format!("rdmd{}", std::env::consts::EXE_SUFFIX)))
}

/// Win32 error for a job that does not allow its processes to break away.
const ERROR_ACCESS_DENIED: i32 = 5;

/// Run `spawn(true)`, asking to leave the caller's job object, and when the job
/// forbids that (access denied) run `spawn(false)` instead. A browser or
/// terminal often runs its children in a job that kills them on exit; leaving
/// it keeps the daemon alive.
#[cfg_attr(unix, allow(dead_code))]
fn with_breakaway_fallback<T>(mut spawn: impl FnMut(bool) -> io::Result<T>) -> io::Result<T> {
    match spawn(true) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => spawn(false),
        other => other,
    }
}

/// Start `rdmd` detached from this terminal so it outlives the calling command.
pub fn spawn_detached(exe: &Path, endpoint: &IpcEndpoint) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.arg("--endpoint")
        .arg(endpoint.to_string())
        // Run from the temp dir so the daemon never pins the caller's directory.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: Ctrl+C in the caller's terminal does not reach the daemon.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        stop_stdio_inheritance();
        with_breakaway_fallback(|breakaway| {
            let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
            if breakaway {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            cmd.creation_flags(flags);
            cmd.spawn()
        })
        .map(|_| ())
        .map_err(|e| cannot_run(exe, e))
    }
    #[cfg(unix)]
    {
        cmd.spawn().map(|_| ()).map_err(|e| cannot_run(exe, e))
    }
}

fn cannot_run(exe: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("cannot run {}: {}", exe.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakaway_is_tried_first_and_kept_when_it_works() {
        let mut tries = Vec::new();
        let result = with_breakaway_fallback(|breakaway| {
            tries.push(breakaway);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(tries, [true]);
    }

    #[test]
    fn access_denied_retries_without_breakaway() {
        let mut tries = Vec::new();
        let result = with_breakaway_fallback(|breakaway| {
            tries.push(breakaway);
            if breakaway {
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED))
            } else {
                Ok("started")
            }
        });
        assert_eq!(result.unwrap(), "started");
        assert_eq!(tries, [true, false]);
    }

    #[test]
    fn other_errors_are_not_retried() {
        let mut tries = 0;
        let result: io::Result<()> = with_breakaway_fallback(|_| {
            tries += 1;
            Err(io::Error::from_raw_os_error(2))
        });
        assert!(result.is_err());
        assert_eq!(tries, 1);
    }
}
