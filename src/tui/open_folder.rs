//! Opening the folder that holds a download, in the system's file manager.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// The program and arguments that show `path` in the file manager: selecting
/// the file where the platform can and it exists, else opening its folder.
pub fn open_command(path: &Path, file_exists: bool) -> (OsString, Vec<OsString>) {
    let folder = path.parent().unwrap_or(path).as_os_str().to_os_string();
    if cfg!(windows) {
        if file_exists {
            // Explorer takes the path after "/select," as the next argument too.
            ("explorer".into(), vec!["/select,".into(), path.into()])
        } else {
            ("explorer".into(), vec![folder])
        }
    } else if cfg!(target_os = "macos") {
        if file_exists {
            ("open".into(), vec!["-R".into(), path.into()])
        } else {
            ("open".into(), vec![folder])
        }
    } else {
        ("xdg-open".into(), vec![folder])
    }
}

/// Show `path` in the file manager without waiting for it.
pub fn open_folder(path: &Path) -> io::Result<()> {
    let (command, args) = open_command(path, path.is_file());
    spawn_detached(command, args)
}

/// Start `command` with no terminal input or output. A thread reaps it when it
/// exits, so it never lingers as a zombie. Errors name the command.
fn spawn_detached(command: OsString, args: Vec<OsString>) -> io::Result<()> {
    let mut child = Command::new(&command)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{} failed: {e}", command.to_string_lossy()),
            )
        })?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    #[cfg(windows)]
    fn open_command_selects_an_existing_file() {
        let path = Path::new(r"C:\Downloads\a file.iso");
        let (command, args) = open_command(path, true);
        assert_eq!(command, "explorer");
        assert_eq!(strings(&args), vec!["/select,", r"C:\Downloads\a file.iso"]);
    }

    #[test]
    #[cfg(windows)]
    fn open_command_opens_the_folder_when_the_file_is_gone() {
        let (command, args) = open_command(Path::new(r"C:\Downloads\a.iso"), false);
        assert_eq!(command, "explorer");
        assert_eq!(strings(&args), vec![r"C:\Downloads"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn open_command_selects_an_existing_file() {
        let (command, args) = open_command(Path::new("/Users/me/a.iso"), true);
        assert_eq!(command, "open");
        assert_eq!(strings(&args), vec!["-R", "/Users/me/a.iso"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn open_command_opens_the_folder_when_the_file_is_gone() {
        let (command, args) = open_command(Path::new("/Users/me/a.iso"), false);
        assert_eq!(command, "open");
        assert_eq!(strings(&args), vec!["/Users/me"]);
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn open_command_selects_an_existing_file() {
        // xdg-open cannot select a file, so it opens the folder holding it.
        let (command, args) = open_command(Path::new("/home/me/Downloads/a.iso"), true);
        assert_eq!(command, "xdg-open");
        assert_eq!(strings(&args), vec!["/home/me/Downloads"]);
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn open_command_opens_the_folder_when_the_file_is_gone() {
        let (command, args) = open_command(Path::new("/home/me/Downloads/a.iso"), false);
        assert_eq!(command, "xdg-open");
        assert_eq!(strings(&args), vec!["/home/me/Downloads"]);
    }

    #[test]
    fn a_command_that_cannot_start_names_itself_in_the_error() {
        let err = spawn_detached("rustdm-no-such-program".into(), Vec::new()).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("rustdm-no-such-program failed: "),
            "{err}"
        );
    }
}
