//! Unpacking downloaded tools: zip files with the `zip` crate (only the
//! entries we want, only safe names), and `.tar.xz` with the system `tar`.

use super::{Tool, ToolError};
use crate::error::Error;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Longest the system `tar` may take on a download of this size.
const TAR_TIMEOUT: Duration = Duration::from_secs(600);

/// Which part of an archive to keep and where it lands.
pub struct Rules<'a> {
    /// Leading folders to drop from every entry's path.
    pub strip_components: usize,
    /// Patterns for the kept entries, after stripping (see `glob_match`).
    pub wanted: &'a [String],
}

/// Whether `text` matches `pattern`, where `*` stands for any run of characters
/// except `/`, and everything else must be equal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => {
                let run = text.iter().take_while(|&&b| b != b'/').count();
                (0..=run).any(|skip| go(rest, &text[skip..]))
            }
            Some((&c, rest)) => text.first() == Some(&c) && go(rest, &text[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// `path` without its first `strip` components, `/`-separated; `None` when it
/// has no more than `strip` components (the top-level folder itself).
fn stripped(path: &Path, strip: usize) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (parts.len() > strip).then(|| parts[strip..].join("/"))
}

struct Wanted {
    index: usize,
    target: PathBuf,
    size: u64,
    unix_mode: Option<u32>,
}

fn unsafe_entry(tool: Tool, name: &str) -> ToolError {
    ToolError::Unusable(format!(
        "the {tool} download has an entry with an unsafe name ({name}); nothing was installed."
    ))
}

fn io_error(tool: Tool, source: io::Error) -> ToolError {
    ToolError::Io { tool, source }
}

/// Unpack `archive` into `dest`, keeping only entries that match `rules`.
///
/// Every entry must have a name `enclosed_name()` accepts (no `..`, no absolute
/// path); one that does not makes the whole archive unusable, even if we would
/// not have kept it. Wanted entries that are symlinks are refused. The free
/// space is checked against the unpacked size before anything is written.
pub fn unpack_zip(tool: Tool, archive: &Path, dest: &Path, rules: &Rules) -> Result<(), ToolError> {
    let file = File::open(archive).map_err(|e| io_error(tool, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ToolError::Unusable(format!("the {tool} download is not a valid zip: {e}")))?;

    let mut wanted = Vec::new();
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .map_err(|e| ToolError::Unusable(format!("the {tool} zip is damaged: {e}")))?;
        let Some(safe) = entry.enclosed_name() else {
            return Err(unsafe_entry(
                tool,
                &entry.name().map(|n| n.into_owned()).unwrap_or_default(),
            ));
        };
        if entry.is_dir() {
            continue;
        }
        let Some(relative) = stripped(&safe, rules.strip_components) else {
            continue;
        };
        if !rules.wanted.iter().any(|p| glob_match(p, &relative)) {
            continue;
        }
        if entry.is_symlink() {
            return Err(unsafe_entry(
                tool,
                &entry.name().map(|n| n.into_owned()).unwrap_or_default(),
            ));
        }
        wanted.push(Wanted {
            index,
            target: dest.join(&relative),
            size: entry.size(),
            unix_mode: entry.unix_mode(),
        });
    }

    let needed: u64 = wanted.iter().map(|w| w.size).sum();
    crate::disk::ensure_disk_space(dest, needed, crate::disk::SAFETY_MARGIN_BYTES)
        .map_err(|e| disk_error(tool, e))?;

    for item in &wanted {
        if let Some(parent) = item.target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_error(tool, e))?;
        }
        let mut entry = zip
            .by_index(item.index)
            .map_err(|e| ToolError::Unusable(format!("the {tool} zip is damaged: {e}")))?;
        let mut out = File::create(&item.target).map_err(|e| io_error(tool, e))?;
        // Never write more than the entry says it holds.
        let copied =
            io::copy(&mut (&mut entry).take(item.size), &mut out).map_err(|e| io_error(tool, e))?;
        if copied != item.size {
            return Err(ToolError::Unusable(format!(
                "the {tool} zip is damaged: an entry ended early"
            )));
        }
        drop(out);
        make_executable_if_needed(&item.target, item.unix_mode);
    }
    Ok(())
}

/// Keep the program runnable on Unix, where zip files record the mode.
#[cfg(unix)]
fn make_executable_if_needed(path: &Path, unix_mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = unix_mode {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777));
    }
}

#[cfg(not(unix))]
fn make_executable_if_needed(_path: &Path, _unix_mode: Option<u32>) {}

/// Mark a file as runnable on Unix; nothing to do on Windows.
#[cfg(unix)]
pub fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
pub fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn disk_error(tool: Tool, err: Error) -> ToolError {
    match err {
        Error::InsufficientDiskSpace {
            target,
            required,
            available,
        } => ToolError::DiskFull {
            tool,
            target,
            needed: required,
            available,
        },
        Error::Io(source) => ToolError::Io { tool, source },
        other => ToolError::Unusable(other.to_string()),
    }
}

/// Unpack a `.tar.xz` with the system `tar`, keeping only entries that match
/// `rules`. `unpacked_bytes` is the expected size, for the free-space check.
pub async fn unpack_tar_xz(
    tool: Tool,
    archive: &Path,
    dest: &Path,
    rules: &Rules<'_>,
    unpacked_bytes: u64,
) -> Result<(), ToolError> {
    crate::disk::ensure_disk_space(dest, unpacked_bytes, crate::disk::SAFETY_MARGIN_BYTES)
        .map_err(|e| disk_error(tool, e))?;
    std::fs::create_dir_all(dest).map_err(|e| io_error(tool, e))?;

    // tar matches member names before stripping, so each pattern gets a `*/`
    // for every folder that will be stripped.
    let prefix = "*/".repeat(rules.strip_components);
    let mut command = tokio::process::Command::new("tar");
    command
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .arg(format!("--strip-components={}", rules.strip_components))
        .arg("--anchored")
        .arg("--wildcards")
        .args(rules.wanted.iter().map(|p| format!("{prefix}{p}")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|e| {
        ToolError::Unusable(format!(
            "could not run `tar` to unpack {tool} ({e}); install tar and try again."
        ))
    })?;
    let output = tokio::time::timeout(TAR_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| ToolError::Unusable(format!("unpacking {tool} took too long")))?
        .map_err(|e| io_error(tool, e))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Unusable(format!(
            "could not unpack {tool}: {}",
            detail.trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o755);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }

    fn rules(strip: usize, wanted: &[&str]) -> (usize, Vec<String>) {
        (strip, wanted.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn globs_match_within_one_folder_only() {
        assert!(glob_match("bin/*.dll", "bin/avcodec-62.dll"));
        assert!(!glob_match("bin/*.dll", "bin/sub/x.dll"));
        assert!(!glob_match("bin/*.dll", "lib/x.dll"));
        assert!(glob_match("lib/*.so*", "lib/libavutil.so.60.26.103"));
        assert!(glob_match("lib/*.so*", "lib/libavutil.so"));
        assert!(!glob_match("lib/*.so*", "lib/libavutil.a"));
        assert!(glob_match("deno.exe", "deno.exe"));
        assert!(!glob_match("deno.exe", "deno.exe.sig"));
    }

    #[test]
    fn only_wanted_entries_are_extracted_and_the_top_folder_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("ff.zip");
        make_zip(
            &zip_path,
            &[
                ("ffmpeg-8/bin/ffmpeg.exe", b"exe"),
                ("ffmpeg-8/bin/avcodec-62.dll", b"dll"),
                ("ffmpeg-8/bin/ffplay.exe", b"unwanted"),
                ("ffmpeg-8/include/x.h", b"unwanted"),
            ],
        );
        let out = dir.path().join("out");
        let (strip, wanted) = rules(1, &["bin/ffmpeg.exe", "bin/*.dll"]);
        unpack_zip(
            Tool::Ffmpeg,
            &zip_path,
            &out,
            &Rules {
                strip_components: strip,
                wanted: &wanted,
            },
        )
        .unwrap();

        assert_eq!(std::fs::read(out.join("bin/ffmpeg.exe")).unwrap(), b"exe");
        assert_eq!(
            std::fs::read(out.join("bin/avcodec-62.dll")).unwrap(),
            b"dll"
        );
        assert!(!out.join("bin/ffplay.exe").exists());
        assert!(!out.join("include").exists());
    }

    #[test]
    fn an_entry_that_escapes_the_folder_makes_the_archive_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("evil.zip");
        make_zip(
            &zip_path,
            &[("deno.exe", b"exe"), ("../escaped.txt", b"evil")],
        );
        let out = dir.path().join("out");
        let (strip, wanted) = rules(0, &["deno.exe", "*"]);
        let err = unpack_zip(
            Tool::Deno,
            &zip_path,
            &out,
            &Rules {
                strip_components: strip,
                wanted: &wanted,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe name"), "{err}");
        assert!(!dir.path().join("escaped.txt").exists());
        assert!(!out.join("deno.exe").exists(), "nothing is written first");
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zip");
        std::fs::write(&path, b"not a zip").unwrap();
        let wanted = vec!["deno".to_string()];
        let err = unpack_zip(
            Tool::Deno,
            &path,
            &dir.path().join("o"),
            &Rules {
                strip_components: 0,
                wanted: &wanted,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a valid zip"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn unpacked_programs_keep_their_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("d.zip");
        make_zip(&zip_path, &[("deno", b"#!/bin/sh\n")]);
        let out = dir.path().join("out");
        let wanted = vec!["deno".to_string()];
        unpack_zip(
            Tool::Deno,
            &zip_path,
            &out,
            &Rules {
                strip_components: 0,
                wanted: &wanted,
            },
        )
        .unwrap();
        let mode = std::fs::metadata(out.join("deno"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tar_xz_is_unpacked_by_the_system_tar() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src/ffmpeg-8/bin");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ffmpeg"), b"exe").unwrap();
        std::fs::write(src.join("ffplay"), b"unwanted").unwrap();
        let archive = dir.path().join("ff.tar.xz");
        let status = std::process::Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path().join("src"))
            .arg("ffmpeg-8")
            .status()
            .unwrap();
        assert!(status.success());

        let out = dir.path().join("out");
        let wanted = vec!["bin/ffmpeg".to_string()];
        unpack_tar_xz(
            Tool::Ffmpeg,
            &archive,
            &out,
            &Rules {
                strip_components: 1,
                wanted: &wanted,
            },
            1000,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(out.join("bin/ffmpeg")).unwrap(), b"exe");
        assert!(!out.join("bin/ffplay").exists());
    }
}
