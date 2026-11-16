//! What rustdm trusts without asking the network: yt-dlp's release key, and the
//! exact ffmpeg and deno builds it installs, each with a built-in SHA-256.
//!
//! # Updating a pin
//!
//! Change `tag`, `asset`, `version` and `sha256` together, then cross-check the
//! hash against the project's own published checksum file, which is what
//! `tests/tools_pins.rs` (`pins_still_resolve`, run by hand before a release) does:
//!
//! - ffmpeg, BtbN `FFmpeg-Builds`, LGPL shared build, month-end tag (kept
//!   long-term; daily tags are deleted after about 14 days). Checksums:
//!   `https://github.com/BtbN/FFmpeg-Builds/releases/download/<tag>/checksums.sha256`
//! - deno, `denoland/deno` release. Checksums:
//!   `https://github.com/denoland/deno/releases/download/<tag>/<asset>.sha256sum`
//!
//! The values below were checked on 2026-11-16 against those files, and the
//! downloaded archives were hashed locally with `sha256sum`.

use super::Tool;

/// yt-dlp's release signing key (Simon Sawicki, 4096-bit RSA).
/// Source: `public.key` in the yt-dlp repository, dearmored with `gpg --dearmor`.
pub const YTDLP_FINGERPRINT: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";
pub const YTDLP_KEY: &[u8] = include_bytes!("keys/yt-dlp.gpg");
/// Where yt-dlp nightly builds are published.
pub const YTDLP_REPO: &str = "yt-dlp/yt-dlp-nightly-builds";
pub const GITHUB_API: &str = "https://api.github.com";
pub const GITHUB_DOWNLOADS: &str = "https://github.com";

/// The systems rustdm has pinned builds for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows64,
    Linux64,
}

impl Platform {
    pub fn current() -> Option<Platform> {
        if cfg!(all(windows, target_arch = "x86_64")) {
            Some(Platform::Windows64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Platform::Linux64)
        } else {
            None
        }
    }

    /// The yt-dlp release asset to use here, as named in `SHA2-256SUMS`.
    pub fn ytdlp_asset(self) -> &'static str {
        match self {
            Platform::Windows64 => "yt-dlp.exe",
            Platform::Linux64 => "yt-dlp_linux",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archive {
    /// The download is the program itself.
    None,
    Zip,
    /// Unpacked with the system `tar -xJf`.
    TarXz,
}

/// A build of ffmpeg or deno that rustdm installs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub version: String,
    /// Path of the download under the downloads host: `owner/repo/releases/download/<tag>/<asset>`.
    pub path: String,
    /// File name of the download, which is also its name in staging.
    pub asset: String,
    pub sha256: String,
    pub archive: Archive,
    /// Leading folders of the archive to drop (the `ffmpeg-n8.1.3-.../` folder).
    pub strip_components: usize,
    /// Which archive entries to keep, relative to the stripped folder. One `*` may
    /// stand for part of the last name (`bin/*.dll`). Ignored for `Archive::None`.
    pub wanted: Vec<String>,
    /// The program, relative to the install folder.
    pub exe: String,
    /// Folder holding the program, relative to the install folder (what
    /// `--ffmpeg-location` wants); empty when it is the install folder itself.
    pub exe_dir: String,
    /// Rough size once unpacked, for the disk-space check of `tar` archives.
    pub unpacked_bytes: u64,
    /// Where the project publishes this asset's checksum (under the downloads
    /// host); used only to cross-check the pin by hand.
    pub published_checksums_path: String,
}

const FFMPEG_TAG: &str = "autobuild-2026-10-31-12-59";
const FFMPEG_BUILD: &str = "ffmpeg-n8.1.3-27-g8dca1586f4";
const FFMPEG_VERSION: &str = "8.1.3";
const DENO_TAG: &str = "v2.9.8";
const DENO_VERSION: &str = "2.9.8";

fn ffmpeg_pin(platform: Platform) -> Pin {
    let (asset, sha256, archive, exe, wanted, unpacked_bytes) = match platform {
        Platform::Windows64 => (
            format!("{FFMPEG_BUILD}-win64-lgpl-shared-8.1.zip"),
            "5cef7bc48e513f3cee7c881e8b2999d835541edb1846c3e89443a67f8f8556ab",
            Archive::Zip,
            "bin/ffmpeg.exe",
            vec!["bin/ffmpeg.exe", "bin/ffprobe.exe", "bin/*.dll"],
            210_000_000,
        ),
        Platform::Linux64 => (
            format!("{FFMPEG_BUILD}-linux64-lgpl-shared-8.1.tar.xz"),
            "493e89a48da02b9a7d217b60f7f60c3c917c9bed73459d8a106e5e987da082ef",
            Archive::TarXz,
            "bin/ffmpeg",
            vec!["bin/ffmpeg", "bin/ffprobe", "lib/*.so*"],
            180_000_000,
        ),
    };
    Pin {
        version: FFMPEG_VERSION.to_string(),
        path: format!("BtbN/FFmpeg-Builds/releases/download/{FFMPEG_TAG}/{asset}"),
        asset,
        sha256: sha256.to_string(),
        archive,
        strip_components: 1,
        wanted: wanted.into_iter().map(String::from).collect(),
        exe: exe.to_string(),
        exe_dir: "bin".to_string(),
        unpacked_bytes,
        published_checksums_path: format!(
            "BtbN/FFmpeg-Builds/releases/download/{FFMPEG_TAG}/checksums.sha256"
        ),
    }
}

fn deno_pin(platform: Platform) -> Pin {
    let (asset, sha256, exe) = match platform {
        Platform::Windows64 => (
            "deno-x86_64-pc-windows-msvc.zip",
            "cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf",
            "deno.exe",
        ),
        Platform::Linux64 => (
            "deno-x86_64-unknown-linux-gnu.zip",
            "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609",
            "deno",
        ),
    };
    Pin {
        version: DENO_VERSION.to_string(),
        path: format!("denoland/deno/releases/download/{DENO_TAG}/{asset}"),
        asset: asset.to_string(),
        sha256: sha256.to_string(),
        archive: Archive::Zip,
        strip_components: 0,
        wanted: vec![exe.to_string()],
        exe: exe.to_string(),
        exe_dir: String::new(),
        unpacked_bytes: 130_000_000,
        published_checksums_path: format!(
            "denoland/deno/releases/download/{DENO_TAG}/{asset}.sha256sum"
        ),
    }
}

/// The pinned build of `tool` for `platform`. yt-dlp is not pinned (it is
/// looked up in its release list), so it has none.
pub fn pin_for(tool: Tool, platform: Platform) -> Option<Pin> {
    match tool {
        Tool::YtDlp => None,
        Tool::Ffmpeg => Some(ffmpeg_pin(platform)),
        Tool::Deno => Some(deno_pin(platform)),
    }
}

/// The command-line flag that makes `tool` print its version and exit.
pub fn version_flag(tool: Tool) -> &'static str {
    match tool {
        Tool::Ffmpeg => "-version",
        Tool::YtDlp | Tool::Deno => "--version",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::checksum::normalize_hash;
    use crate::tools::openpgp::{parse_fingerprint, PinnedKey};

    #[test]
    fn the_pinned_ytdlp_key_has_the_published_fingerprint() {
        let fingerprint = parse_fingerprint(YTDLP_FINGERPRINT).unwrap();
        let key = PinnedKey::load(YTDLP_KEY, &fingerprint).unwrap();
        assert_eq!(key.fingerprint_hex(), YTDLP_FINGERPRINT);
    }

    #[test]
    fn every_pin_is_well_formed() {
        for platform in [Platform::Windows64, Platform::Linux64] {
            for tool in [Tool::Ffmpeg, Tool::Deno] {
                let pin = pin_for(tool, platform).unwrap();
                assert_eq!(
                    normalize_hash(&pin.sha256).as_deref(),
                    Some(pin.sha256.as_str()),
                    "{tool} on {platform:?}: hash is not lower-case SHA-256 hex"
                );
                assert!(pin.path.ends_with(&pin.asset), "{tool}: {}", pin.path);
                assert!(
                    pin.wanted.iter().any(|w| *w == pin.exe),
                    "{tool}: exe is not kept"
                );
                assert!(pin.exe.starts_with(&pin.exe_dir));
                assert!(!pin.version.is_empty());
            }
        }
        assert!(pin_for(Tool::YtDlp, Platform::Linux64).is_none());
    }

    #[test]
    fn ffmpeg_is_the_lgpl_shared_build_with_month_end_tag() {
        let pin = pin_for(Tool::Ffmpeg, Platform::Windows64).unwrap();
        assert!(pin.asset.contains("lgpl-shared"));
        assert!(pin.path.contains("autobuild-2026-10-31-"));
        assert_eq!(pin.archive, Archive::Zip);
        assert_eq!(
            pin_for(Tool::Ffmpeg, Platform::Linux64).unwrap().archive,
            Archive::TarXz
        );
    }

    #[test]
    fn each_platform_names_its_ytdlp_asset() {
        assert_eq!(Platform::Windows64.ytdlp_asset(), "yt-dlp.exe");
        assert_eq!(Platform::Linux64.ytdlp_asset(), "yt-dlp_linux");
        assert_eq!(version_flag(Tool::Ffmpeg), "-version");
        assert_eq!(version_flag(Tool::Deno), "--version");
    }
}
