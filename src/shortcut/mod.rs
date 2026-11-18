//! `rdm shortcut`: Desktop and Start menu shortcuts that open the dashboard.
//!
//! The shortcuts are ordinary Windows `.lnk` files written through the Shell's
//! own `IShellLinkW` (see `windows.rs`), so Explorer shows them like any other
//! program's. Everything here outside `windows.rs` is plain path logic that is
//! tested on every platform.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows;

/// The file name used in each place; `rdm shortcut remove` deletes only this.
pub const LINK_FILE: &str = "rustdm.lnk";
/// The tooltip Explorer shows for the shortcut.
pub const DESCRIPTION: &str = "rustdm download manager";

/// Where a shortcut goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Desktop,
    StartMenu,
}

impl Place {
    pub const ALL: [Place; 2] = [Place::Desktop, Place::StartMenu];

    pub fn label(self) -> &'static str {
        match self {
            Place::Desktop => "Desktop",
            Place::StartMenu => "Start menu",
        }
    }
}

/// What a shortcut points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The `rdm.exe` that opens the dashboard; its own icon is the shortcut's.
    pub exe: PathBuf,
    /// The folder the dashboard starts in: the one holding `rdm.exe`.
    pub working_dir: PathBuf,
}

impl Target {
    pub fn for_exe(exe: &Path) -> Target {
        let working_dir = match exe.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Target {
            exe: exe.to_path_buf(),
            working_dir,
        }
    }
}

/// One place's shortcut as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub place: Place,
    pub link: PathBuf,
    /// `None` when there is no shortcut there; otherwise what it points at,
    /// which is empty if Windows could not read the shortcut.
    pub points_to: Option<PathBuf>,
}

/// The shortcut file inside a place's folder.
pub fn link_in(folder: &Path) -> PathBuf {
    folder.join(LINK_FILE)
}

/// The line `rdm shortcut status` prints for one place.
pub fn describe(found: &Found, this_exe: &Path) -> String {
    let place = found.place.label();
    match &found.points_to {
        None => format!("{place}: no shortcut"),
        Some(target) if target.as_os_str().is_empty() => {
            format!("{place}: {} (cannot be read)", found.link.display())
        }
        Some(target) if target == this_exe => {
            format!("{place}: {} -> {}", found.link.display(), target.display())
        }
        Some(target) => format!(
            "{place}: {} -> {} (not this rdm; run `rdm shortcut install` to point it here)",
            found.link.display(),
            target.display()
        ),
    }
}

#[cfg(not(windows))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "shortcuts are made on Windows only",
    )
}

/// Create (or replace) the shortcut in every place. Returns the files written.
pub fn install(target: &Target) -> io::Result<Vec<(Place, PathBuf)>> {
    #[cfg(windows)]
    {
        windows::install(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        Err(unsupported())
    }
}

/// Delete the shortcut from every place. Returns the files removed.
pub fn remove() -> io::Result<Vec<(Place, PathBuf)>> {
    #[cfg(windows)]
    {
        windows::remove()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

/// What is in each place now.
pub fn status() -> io::Result<Vec<Found>> {
    #[cfg(windows)]
    {
        windows::status()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

/// The `rdm shortcut` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Create the shortcuts (run again after moving the program folder)
    Install,
    /// Delete the shortcuts
    Remove,
    /// Show the shortcuts and what they open
    Status,
}

/// Run one `rdm shortcut` subcommand for the `rdm` at `this_exe`, printing
/// what was done to `out`.
pub fn run(action: Action, this_exe: &Path, out: &mut impl std::io::Write) -> io::Result<()> {
    match action {
        Action::Install => {
            for (place, link) in install(&Target::for_exe(this_exe))? {
                writeln!(out, "{} shortcut: {}", place.label(), link.display())?;
            }
            writeln!(out, "Double-click rustdm to open the dashboard.")?;
        }
        Action::Remove => {
            let removed = remove()?;
            if removed.is_empty() {
                writeln!(out, "No rustdm shortcuts to remove.")?;
            }
            for (place, link) in removed {
                writeln!(
                    out,
                    "Removed the {} shortcut: {}",
                    place.label(),
                    link.display()
                )?;
            }
        }
        Action::Status => {
            for found in status()? {
                writeln!(out, "{}", describe(&found, this_exe))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(points_to: Option<&str>) -> Found {
        Found {
            place: Place::Desktop,
            link: PathBuf::from("D/rustdm.lnk"),
            points_to: points_to.map(PathBuf::from),
        }
    }

    #[test]
    fn the_target_starts_in_the_folder_that_holds_rdm() {
        let target = Target::for_exe(Path::new("/opt/rdm/rdm.exe"));
        assert_eq!(target.exe, PathBuf::from("/opt/rdm/rdm.exe"));
        assert_eq!(target.working_dir, PathBuf::from("/opt/rdm"));
    }

    #[test]
    fn a_bare_exe_name_starts_in_the_current_folder() {
        assert_eq!(
            Target::for_exe(Path::new("rdm.exe")).working_dir,
            PathBuf::from(".")
        );
    }

    #[test]
    fn every_place_uses_the_same_file_name() {
        assert_eq!(
            link_in(Path::new("/desk")),
            PathBuf::from("/desk/rustdm.lnk")
        );
    }

    #[test]
    fn status_names_the_place_and_what_the_shortcut_opens() {
        let exe = Path::new("C/rdm.exe");
        assert_eq!(describe(&found(None), exe), "Desktop: no shortcut");
        assert_eq!(
            describe(&found(Some("C/rdm.exe")), exe),
            "Desktop: D/rustdm.lnk -> C/rdm.exe"
        );
        assert_eq!(
            describe(&found(Some("")), exe),
            "Desktop: D/rustdm.lnk (cannot be read)"
        );
    }

    #[test]
    fn status_points_out_a_shortcut_to_another_copy_of_rdm() {
        let line = describe(&found(Some("Old/rdm.exe")), Path::new("C/rdm.exe"));
        assert!(line.ends_with("(not this rdm; run `rdm shortcut install` to point it here)"));
    }

    #[test]
    fn places_have_readable_names() {
        let labels: Vec<_> = Place::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(labels, ["Desktop", "Start menu"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn other_systems_are_told_shortcuts_are_windows_only() {
        let err = install(&Target::for_exe(Path::new("rdm"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert_eq!(remove().unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(status().unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
