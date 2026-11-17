//! A pretend release host for the tools manager: serve a yt-dlp release the
//! way the real one is laid out, signed by the test key, and break any part of it.

use super::tools::{sign_sums, test_pinned_key};
use super::{Route, TestServer};
use crate::tools::checksum::sha256_hex;
use crate::tools::release::Sources;
use std::collections::HashMap;

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

/// A fake program that prints `version` and exits 0, for any arguments. On
/// Unix it is a shell script; Windows has no shell, so it is `rdmd` itself,
/// which answers `--version`.
pub fn fake_program(version: &str) -> Vec<u8> {
    if cfg!(windows) {
        let rdmd = std::env::var_os("CARGO_BIN_EXE_rdmd")
            .expect("CARGO_BIN_EXE_rdmd is set when integration tests run");
        let mut bytes = std::fs::read(rdmd).expect("read rdmd");
        // Make each version's file different.
        bytes.extend_from_slice(version.as_bytes());
        bytes
    } else {
        format!("#!/bin/sh\necho {version}\n").into_bytes()
    }
}

/// A fake program that exits with `code`, for any arguments (Unix only).
pub fn failing_program(code: i32) -> Vec<u8> {
    format!("#!/bin/sh\nexit {code}\n").into_bytes()
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
    let sig = sign_sums(&sums);
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
