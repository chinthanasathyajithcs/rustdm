//! Well-known folders.

use std::path::{Path, PathBuf};

/// Shown when a folder typed into the TUI isn't a full path.
const NOT_FULL: &str = "Enter a full path, such as ~/Downloads";

/// The user's home folder: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// The user's Downloads folder: `%USERPROFILE%\Downloads` on Windows,
/// `$HOME/Downloads` elsewhere, or a relative `Downloads` when neither is set.
pub fn default_downloads_dir() -> PathBuf {
    home_dir().map_or_else(|| PathBuf::from("Downloads"), |h| h.join("Downloads"))
}

/// A folder typed into the TUI as a full path. A leading `~` is `home`; any
/// other relative path is refused, because the daemon runs in another folder
/// and a path relative to wherever the TUI started is rarely what was meant.
pub fn full_folder(text: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Enter a folder".into());
    }
    let after_tilde = if text == "~" {
        Some("")
    } else {
        text.strip_prefix("~/")
            .or_else(|| text.strip_prefix("~\\").filter(|_| cfg!(windows)))
    };
    let path = match after_tilde {
        Some(rest) => {
            let home = home.ok_or("No home folder is set, so ~ can't be used")?;
            home.join(rest)
        }
        None => PathBuf::from(text),
    };
    if !path.has_root() {
        return Err(NOT_FULL.into());
    }
    // On Windows a path like \Videos has a root but no drive; this adds it.
    std::path::absolute(&path).map_err(|e| format!("Can't use this folder: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_leading_tilde_is_the_home_folder() {
        let home = Path::new(if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        });
        assert_eq!(full_folder("~", Some(home)).unwrap(), home);
        assert_eq!(
            full_folder("~/Videos", Some(home)).unwrap(),
            home.join("Videos")
        );
        assert_eq!(
            full_folder("  ~/Videos ", Some(home)).unwrap(),
            home.join("Videos")
        );
    }

    #[test]
    fn a_relative_folder_is_refused() {
        let home = Path::new("/home/me");
        for text in ["Videos", "./Videos", "~other/Videos"] {
            assert_eq!(
                full_folder(text, Some(home)).unwrap_err(),
                "Enter a full path, such as ~/Downloads",
                "{text}"
            );
        }
        assert_eq!(full_folder("  ", Some(home)).unwrap_err(), "Enter a folder");
    }

    #[test]
    fn a_tilde_without_a_home_folder_is_refused() {
        assert!(full_folder("~/Videos", None).is_err());
    }

    #[test]
    fn full_paths_are_kept() {
        let full = if cfg!(windows) {
            r"D:\Downloads"
        } else {
            "/srv/downloads"
        };
        assert_eq!(full_folder(full, None).unwrap(), PathBuf::from(full));
    }
}
