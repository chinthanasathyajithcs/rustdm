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
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        stop_stdio_inheritance();
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run {}: {}", exe.display(), e)))
}
