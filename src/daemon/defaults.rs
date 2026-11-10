//! Defaults for new downloads: resolving the built-in values and checking
//! what a client asks to change.

use crate::dirs::default_downloads_dir;
use crate::engine::{DEFAULT_CONNECTIONS, MAX_CONNECTIONS};
use crate::ipc::protocol::{
    DownloadDefaults, RpcError, SavedDefaults, SetDefaultsParams, CODE_INVALID_PARAMS,
};
use std::path::Path;

/// What `RpcError::invalid_params` puts before its detail.
const INVALID_PARAMS_PREFIX: &str = "Invalid params: ";

/// An invalid-params error whose message starts with the field at fault, so a
/// client can show it under that field.
fn field_error(field: &str, detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, format!("{field}: {detail}"), None)
}

/// The saved defaults with every unset field filled in.
pub fn resolve(saved: &SavedDefaults) -> DownloadDefaults {
    DownloadDefaults {
        folder: saved.folder.clone().unwrap_or_else(default_downloads_dir),
        connections: saved.connections.unwrap_or(DEFAULT_CONNECTIONS),
        max_bytes_per_sec: saved.max_bytes_per_sec,
    }
}

/// `saved` with the fields `params` gives, after checking each of them.
/// `check_speed` is the daemon's own speed rule.
pub fn updated(
    saved: &SavedDefaults,
    params: SetDefaultsParams,
    check_speed: impl Fn(u64) -> Result<(), RpcError>,
) -> Result<SavedDefaults, RpcError> {
    if let Some(n) = params.connections {
        if !(1..=MAX_CONNECTIONS).contains(&n) {
            return Err(field_error(
                "connections",
                format!("must be between 1 and {MAX_CONNECTIONS}"),
            ));
        }
    }
    if let Some(folder) = &params.folder {
        check_folder(folder)?;
    }
    if let Some(bps) = params.max_bytes_per_sec {
        check_speed(bps).map_err(|e| {
            let detail = e
                .message
                .strip_prefix(INVALID_PARAMS_PREFIX)
                .unwrap_or(&e.message);
            field_error("speed", detail)
        })?;
    }
    Ok(SavedDefaults {
        folder: params.folder.or_else(|| saved.folder.clone()),
        connections: params.connections.or(saved.connections),
        max_bytes_per_sec: params.max_bytes_per_sec.unwrap_or(saved.max_bytes_per_sec),
    })
}

/// A default folder must be a full path. If it exists it must be a folder the
/// daemon can write in; a missing one is created when a download needs it.
fn check_folder(folder: &Path) -> Result<(), RpcError> {
    if !folder.is_absolute() {
        return Err(field_error("folder", "must be a full path"));
    }
    if !folder.exists() {
        return Ok(());
    }
    if !folder.is_dir() {
        return Err(field_error("folder", "is a file, not a folder"));
    }
    let probe = folder.join(format!(".rdm-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|e| field_error("folder", format!("can't write there ({e})")))
}
