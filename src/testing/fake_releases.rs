//! A pretend release host for the tools manager: serve a yt-dlp release the
//! way the real one is laid out, signed by the test key, and break any part of it.

use super::tools::{sign, test_pinned_key, SigSpec, TEST_CREATION_TIME};
use super::{Route, TestServer};
use crate::tools::checksum::sha256_hex;
use crate::tools::pins::{Archive, Pin};
use crate::tools::release::Sources;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

/// The signing time of the next release published without one: every release
/// is signed a little later than the one before, as real ones are.
static NEXT_SIGNED_AT: AtomicU32 = AtomicU32::new(TEST_CREATION_TIME);

/// The repository the test sources look yt-dlp up in.
pub const TEST_REPO: &str = "yt-dlp/yt-dlp-nightly-builds";
/// The yt-dlp asset name the test sources ask for.
pub const TEST_ASSET: &str = "yt-dlp_test";
/// Path of the "latest release" record on a server set up by `publish_ytdlp`.
pub const LATEST_PATH: &str = "/repos/yt-dlp/yt-dlp-nightly-builds/releases/latest";

/// Sources that fetch from `server` and trust the test key. No pins are set.
pub fn test_sources(server: &TestServer) -> Sources {
    let base = server.url().as_str().trim_end_matches('/').to_string();
    Sources {
        api_base: base.clone(),
        download_base: base,
        ytdlp_repo: TEST_REPO.to_string(),
        keys: vec![test_pinned_key()],
        ytdlp_min_version: "2026.01.01".to_string(),
        ytdlp_asset: Some(TEST_ASSET.to_string()),
        pins: HashMap::new(),
    }
}

/// Marks the version a fake program built from `rdmd` prints.
pub(super) use super::tool_program::VERSION_MARKER as FAKE_VERSION_MARKER;

/// The version appended to a fake program (see `fake_program`), if it has one.
/// The version ends at a NUL byte, so a test may pad the program after it.
pub fn fake_version_in(program: &[u8]) -> Option<String> {
    super::tool_program::version_in(program)
}

/// For `rdmd` in a test build: when it was started as a fake program (see
/// `fake_program`) with just `--version`, print that version and say so.
pub fn answer_version_if_fake() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--version") || args.next().is_some() {
        return false;
    }
    let version = std::env::current_exe()
        .and_then(std::fs::read)
        .ok()
        .and_then(|bytes| fake_version_in(&bytes));
    match version {
        Some(version) => {
            let _ = writeln!(std::io::stdout(), "{version}");
            true
        }
        None => false,
    }
}

/// A fake program that prints `version` and exits 0, for any arguments. On
/// Unix it is a shell script; Windows has no shell, so it is `rdmd` itself
/// with the version appended, which `rdmd --version` prints (see
/// `answer_version_if_fake`).
pub fn fake_program(version: &str) -> Vec<u8> {
    if cfg!(windows) {
        let rdmd = std::env::var_os("CARGO_BIN_EXE_rdmd")
            .expect("CARGO_BIN_EXE_rdmd is set when integration tests run");
        let mut bytes = std::fs::read(rdmd).expect("read rdmd");
        // Makes each version's file different, and is what `--version` prints.
        bytes.extend_from_slice(FAKE_VERSION_MARKER);
        bytes.extend_from_slice(version.as_bytes());
        bytes.push(0);
        bytes
    } else {
        format!("#!/bin/sh\necho {version}\n").into_bytes()
    }
}

/// A fake program that exits with `code`, for any arguments (Unix only).
pub fn failing_program(code: i32) -> Vec<u8> {
    format!("#!/bin/sh\nexit {code}\n").into_bytes()
}

/// A zip file holding `entries` (name and contents), as a pinned tool archive.
pub fn zip_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    zip_with(entries, zip::CompressionMethod::Deflated)
}

/// Like `zip_bytes`, but without compressing: a big entry (a copy of a test
/// program) would take many seconds to deflate in a debug build.
pub fn zip_bytes_stored(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    zip_with(entries, zip::CompressionMethod::Stored)
}

fn zip_with(entries: &[(&str, Vec<u8>)], method: zip::CompressionMethod) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o755);
    for (name, data) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A pin for `archive`, to be served at `/pins/tool.zip`.
pub fn zip_pin(
    archive: &[u8],
    version: &str,
    strip: usize,
    wanted: &[&str],
    exe: &str,
    exe_dir: &str,
) -> Pin {
    Pin {
        version: version.to_string(),
        path: "pins/tool.zip".to_string(),
        asset: "tool.zip".to_string(),
        sha256: sha256_hex(archive),
        archive: Archive::Zip,
        strip_components: strip,
        wanted: wanted.iter().map(|s| s.to_string()).collect(),
        exe: exe.to_string(),
        exe_dir: exe_dir.to_string(),
        unpacked_bytes: 1000,
        published_checksums_path: String::new(),
    }
}

/// What `publish_ytdlp_with` should serve, so a test can break one part of a release.
pub struct ReleaseSpec {
    pub version: String,
    /// The yt-dlp download itself.
    pub asset: Vec<u8>,
    /// The hash listed in `SHA2-256SUMS`; `None` means the real hash of `asset`.
    pub listed_sha256: Option<String>,
    /// Change the checksum file after it was signed.
    pub tamper_after_signing: bool,
    /// When the signature says it was made (unix seconds); `None` means a
    /// time later than any earlier release published this way.
    pub signed_at: Option<u32>,
}

/// Serve a yt-dlp release on `server`, which must have been started with
/// `TestServer::start_routes`: the release record, the checksum file, its
/// signature by the test key, and the download. Calling it again replaces the
/// release, like a new nightly.
pub fn publish_ytdlp(server: &TestServer, version: &str, asset: &[u8]) {
    publish_ytdlp_with(
        server,
        ReleaseSpec {
            version: version.to_string(),
            asset: asset.to_vec(),
            listed_sha256: None,
            tamper_after_signing: false,
            signed_at: None,
        },
    );
}

pub fn publish_ytdlp_with(server: &TestServer, spec: ReleaseSpec) {
    let base = server.url().as_str().trim_end_matches('/').to_string();
    let download = format!("/{TEST_REPO}/releases/download/{}", spec.version);
    let listed = spec
        .listed_sha256
        .clone()
        .unwrap_or_else(|| sha256_hex(&spec.asset));
    let sums = format!("{listed}  {TEST_ASSET}\n").into_bytes();
    let signed_at = spec
        .signed_at
        .unwrap_or_else(|| NEXT_SIGNED_AT.fetch_add(60, Ordering::SeqCst));
    let sig = sign(
        &sums,
        &SigSpec {
            creation_time: Some(signed_at),
            ..SigSpec::default()
        },
    );
    let served_sums = if spec.tamper_after_signing {
        format!("{}  {TEST_ASSET}\n", "0".repeat(64)).into_bytes()
    } else {
        sums
    };
    let url_of = |name: &str| format!("{base}{download}/{name}");
    let record = serde_json::json!({
        "tag_name": spec.version,
        "assets": [
            {"name": "SHA2-256SUMS", "browser_download_url": url_of("SHA2-256SUMS")},
            {"name": "SHA2-256SUMS.sig", "browser_download_url": url_of("SHA2-256SUMS.sig")},
            {"name": TEST_ASSET, "browser_download_url": url_of(TEST_ASSET)},
        ]
    });
    server.set_route(LATEST_PATH, Route::ok(record.to_string()));
    server.set_route(&format!("{download}/SHA2-256SUMS"), Route::ok(served_sums));
    server.set_route(&format!("{download}/SHA2-256SUMS.sig"), Route::ok(sig));
    server.set_route(&format!("{download}/{TEST_ASSET}"), Route::ok(spec.asset));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fake_windows_program_carries_its_version_at_the_end() {
        let mut program = b"MZ....rdmd bytes".to_vec();
        program.extend_from_slice(FAKE_VERSION_MARKER);
        program.extend_from_slice(b"2026.11.14.200000");
        assert_eq!(
            fake_version_in(&program).as_deref(),
            Some("2026.11.14.200000")
        );
        assert_eq!(fake_version_in(b"plain program"), None);
    }

    #[test]
    fn padding_after_a_fake_programs_version_is_not_part_of_it() {
        let mut program = b"MZ....rdmd bytes".to_vec();
        program.extend_from_slice(FAKE_VERSION_MARKER);
        program.extend_from_slice(b"2026.11.14.200000\0");
        program.extend(std::iter::repeat_n(b'#', 1000));
        assert_eq!(
            fake_version_in(&program).as_deref(),
            Some("2026.11.14.200000")
        );
    }
}
