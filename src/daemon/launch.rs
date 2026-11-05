use crate::ipc::transport::IpcEndpoint;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run {}: {}", exe.display(), e)))
}
