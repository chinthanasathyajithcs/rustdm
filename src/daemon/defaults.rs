//! Defaults for new downloads: resolving the built-in values and checking
//! what a client asks to change.

use crate::daemon::folder_check;
use crate::dirs::default_downloads_dir;
use crate::engine::{DEFAULT_CONNECTIONS, MAX_CONNECTIONS};
use crate::ipc::protocol::{
    DownloadDefaults, RpcError, SavedDefaults, SetDefaultsParams, CODE_INVALID_PARAMS,
};

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

/// `saved` with the fields `params` gives, after checking each of them except
/// the folder (see `check_folder`). `check_speed` is the daemon's own speed rule.
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

/// Check the folder `params` asks for, if any. It touches the disk, so the
/// daemon runs it before taking its state lock.
pub fn check_folder(params: &SetDefaultsParams) -> Result<(), RpcError> {
    match &params.folder {
        Some(folder) => folder_check::check_folder(folder).map_err(|e| field_error("folder", e)),
        None => Ok(()),
    }
}
