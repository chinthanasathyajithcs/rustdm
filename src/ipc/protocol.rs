use crate::video::{ChosenQuality, VideoChoice};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const JSONRPC_VERSION: &str = "2.0";

// --- Standard JSON-RPC 2.0 Error Codes ---
pub const CODE_PARSE_ERROR: i32 = -32700;
pub const CODE_INVALID_REQUEST: i32 = -32600;
pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
pub const CODE_INVALID_PARAMS: i32 = -32602;
pub const CODE_INTERNAL_ERROR: i32 = -32603;

// --- Download Lifecycle & State Error Codes (-32000..-32019) ---
pub const CODE_DOWNLOAD_NOT_FOUND: i32 = -32001;
pub const CODE_CANNOT_PAUSE_COMPLETED: i32 = -32002;
pub const CODE_ALREADY_COMPLETED: i32 = -32003;
pub const CODE_OPERATION_IN_PROGRESS: i32 = -32004;
pub const CODE_DUPLICATE_DOWNLOAD: i32 = -32005;

// --- Storage & Filesystem Error Codes (-32020..-32039) ---
pub const CODE_INSUFFICIENT_DISK_SPACE: i32 = -32020;
pub const CODE_DISK_FULL_MID_FLIGHT: i32 = -32021;
pub const CODE_DESTINATION_EXISTS: i32 = -32022;
pub const CODE_INVALID_PATH: i32 = -32023;
pub const CODE_PERMISSION_DENIED: i32 = -32024;
pub const CODE_FILE_LOCKED: i32 = -32025;
pub const CODE_CORRUPT_STATE_FILE: i32 = -32026;

// --- Network & Protocol Error Codes (-32040..-32059) ---
pub const CODE_NETWORK_ERROR: i32 = -32040;
pub const CODE_LINK_EXPIRED: i32 = -32041;
pub const CODE_FILE_CHANGED_ON_SERVER: i32 = -32042;
pub const CODE_SERVER_LOST_RANGE_SUPPORT: i32 = -32043;
pub const CODE_RATE_LIMITED: i32 = -32044;
pub const CODE_TOO_MANY_REDIRECTS: i32 = -32045;

// --- Daemon Lifecycle Error Codes (-32060..-32079) ---
pub const CODE_DAEMON_ALREADY_RUNNING: i32 = -32060;
pub const CODE_DAEMON_SHUTTING_DOWN: i32 = -32061;

// --- Tools Manager Error Codes (-32080..-32099) ---
pub const CODE_TOOL_FAILED: i32 = -32080;

/// JSON-RPC Message identifier (either integer or string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

/// JSON-RPC 2.0 Request frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub fn new(id: RpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 Notification frame (no `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 Error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    // Standard JSON-RPC errors
    pub fn parse_error(detail: Option<String>) -> Self {
        Self::new(
            CODE_PARSE_ERROR,
            "Parse error: invalid JSON was received by the server",
            detail.map(Value::String),
        )
    }

    pub fn invalid_request(detail: Option<String>) -> Self {
        Self::new(
            CODE_INVALID_REQUEST,
            "Invalid Request: the JSON sent is not a valid Request object",
            detail.map(Value::String),
        )
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            CODE_METHOD_NOT_FOUND,
            format!("Method not found: '{}'", method),
            None,
        )
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(
            CODE_INVALID_PARAMS,
            format!("Invalid params: {}", detail.into()),
            None,
        )
    }

    pub fn internal_error(detail: impl Into<String>) -> Self {
        Self::new(
            CODE_INTERNAL_ERROR,
            format!("Internal error: {}", detail.into()),
            None,
        )
    }

    // Domain errors
    pub fn download_not_found(id: &str) -> Self {
        Self::new(
            CODE_DOWNLOAD_NOT_FOUND,
            format!("Download not found: '{}'", id),
            Some(serde_json::json!({ "id": id })),
        )
    }

    pub fn cannot_pause_completed(id: &str) -> Self {
        Self::new(
            CODE_CANNOT_PAUSE_COMPLETED,
            format!(
                "Cannot pause download '{}' because it is already completed",
                id
            ),
            Some(serde_json::json!({ "id": id })),
        )
    }

    pub fn already_completed(id: &str) -> Self {
        Self::new(
            CODE_ALREADY_COMPLETED,
            format!("Download '{}' is already completed", id),
            Some(serde_json::json!({ "id": id })),
        )
    }

    pub fn operation_in_progress(id: &str, operation: &str) -> Self {
        Self::new(
            CODE_OPERATION_IN_PROGRESS,
            format!(
                "Operation '{}' already in progress for download '{}'",
                operation, id
            ),
            Some(serde_json::json!({ "id": id, "operation": operation })),
        )
    }

    pub fn duplicate_download(url: &str, existing_id: &str) -> Self {
        Self::new(
            CODE_DUPLICATE_DOWNLOAD,
            format!(
                "URL '{}' is already being downloaded as '{}'",
                url, existing_id
            ),
            Some(serde_json::json!({ "url": url, "existing_id": existing_id })),
        )
    }

    pub fn insufficient_disk_space(required_bytes: u64, available_bytes: u64) -> Self {
        Self::new(
            CODE_INSUFFICIENT_DISK_SPACE,
            format!(
                "Insufficient disk space: required {} bytes, available {} bytes",
                required_bytes, available_bytes
            ),
            Some(serde_json::json!({
                "required_bytes": required_bytes,
                "available_bytes": available_bytes
            })),
        )
    }

    pub fn disk_full_mid_flight(id: &str) -> Self {
        Self::new(
            CODE_DISK_FULL_MID_FLIGHT,
            format!(
                "Disk became full while downloading '{}'. Download paused.",
                id
            ),
            Some(serde_json::json!({ "id": id })),
        )
    }

    pub fn destination_exists(path: &std::path::Path) -> Self {
        Self::new(
            CODE_DESTINATION_EXISTS,
            format!("Destination file already exists: '{}'", path.display()),
            Some(serde_json::json!({ "path": path.to_string_lossy() })),
        )
    }

    pub fn invalid_path(path: &std::path::Path, reason: &str) -> Self {
        Self::new(
            CODE_INVALID_PATH,
            format!("Invalid destination path '{}': {}", path.display(), reason),
            Some(serde_json::json!({ "path": path.to_string_lossy(), "reason": reason })),
        )
    }

    pub fn permission_denied(path: &std::path::Path) -> Self {
        Self::new(
            CODE_PERMISSION_DENIED,
            format!("Permission denied accessing '{}'", path.display()),
            Some(serde_json::json!({ "path": path.to_string_lossy() })),
        )
    }

    pub fn file_locked(path: &std::path::Path) -> Self {
        Self::new(
            CODE_FILE_LOCKED,
            format!("File is locked by another process: '{}'", path.display()),
            Some(serde_json::json!({ "path": path.to_string_lossy() })),
        )
    }

    pub fn corrupt_state_file(path: &std::path::Path, reason: &str) -> Self {
        Self::new(
            CODE_CORRUPT_STATE_FILE,
            format!("Corrupt state file '{}': {}", path.display(), reason),
            Some(serde_json::json!({ "path": path.to_string_lossy(), "reason": reason })),
        )
    }

    pub fn network_error(detail: impl Into<String>) -> Self {
        let msg = detail.into();
        Self::new(
            CODE_NETWORK_ERROR,
            format!("Network error: {}", msg),
            Some(serde_json::json!({ "detail": msg })),
        )
    }

    pub fn link_expired(id: &str, url: &str) -> Self {
        Self::new(
            CODE_LINK_EXPIRED,
            format!("Download link expired (HTTP 403/410) for download '{}'", id),
            Some(serde_json::json!({ "id": id, "url": url })),
        )
    }

    pub fn file_changed_on_server(
        id: &str,
        expected_validator: &str,
        actual_validator: &str,
    ) -> Self {
        Self::new(
            CODE_FILE_CHANGED_ON_SERVER,
            format!("File changed on remote server for download '{}'", id),
            Some(serde_json::json!({
                "id": id,
                "expected": expected_validator,
                "actual": actual_validator
            })),
        )
    }

    pub fn server_lost_range_support(id: &str) -> Self {
        Self::new(
            CODE_SERVER_LOST_RANGE_SUPPORT,
            format!(
                "Remote server refused byte-ranges on resume (returned 200 OK) for download '{}'",
                id
            ),
            Some(serde_json::json!({ "id": id })),
        )
    }

    pub fn rate_limited(retry_after_secs: Option<u64>) -> Self {
        Self::new(
            CODE_RATE_LIMITED,
            format!(
                "Server rate-limited request (429/503){}",
                retry_after_secs
                    .map(|s| format!("; retry after {}s", s))
                    .unwrap_or_default()
            ),
            Some(serde_json::json!({ "retry_after_secs": retry_after_secs })),
        )
    }

    pub fn too_many_redirects(url: &str) -> Self {
        Self::new(
            CODE_TOO_MANY_REDIRECTS,
            format!("Too many redirects following '{}'", url),
            Some(serde_json::json!({ "url": url })),
        )
    }

    pub fn daemon_already_running() -> Self {
        Self::new(
            CODE_DAEMON_ALREADY_RUNNING,
            "Another rustdm daemon instance is already running",
            None,
        )
    }

    /// A tools-manager request failed; `message` is already fit to show the user.
    pub fn tool_failed(message: impl Into<String>) -> Self {
        Self::new(CODE_TOOL_FAILED, message, None)
    }

    pub fn daemon_shutting_down() -> Self {
        Self::new(
            CODE_DAEMON_SHUTTING_DOWN,
            "Daemon is currently shutting down and not accepting new tasks",
            None,
        )
    }
}

/// JSON-RPC 2.0 Response frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: RpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn success(id: RpcId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: RpcId, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Top-level message enum returned by line parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(RpcRequest),
    Response(RpcResponse),
    Notification(RpcNotification),
}

// --- Parameter & Result Payloads ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingResult {
    pub version: String,
    pub uptime_secs: u64,
    /// Folder holding the daemon's queue and log; absent for an in-memory daemon
    /// or one from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownResult {
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddDownloadParams {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<usize>,
    /// Speed cap for this download alone in bytes per second; 0 or missing means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_sec: Option<u64>,
    /// Cookies, Referer and User-Agent to send with every request, from a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<crate::request_context::RequestContext>,
    /// For a video page: `best`, `audio` or a height such as `1080p`. Left out,
    /// the user is asked to choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
}

/// `download.choose_quality`: pick the quality of a download that is `choosing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChooseQualityParams {
    pub id: String,
    /// The `id` of one of the download's choices.
    pub choice_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadIdParam {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelDownloadParams {
    pub id: String,
    pub delete_partial: bool,
}

/// Where `download.move` puts a download in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueuePosition {
    /// First in the queue.
    Top,
    /// One place earlier.
    Up,
    /// One place later.
    Down,
    /// Last in the queue.
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveDownloadParams {
    pub id: String,
    pub position: QueuePosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDownloadSpeedParams {
    pub id: String,
    /// Speed cap for this download in bytes per second; 0 removes it.
    pub max_bytes_per_sec: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateUrlParams {
    pub id: String,
    pub new_url: String,
}

/// Daemon-wide queue limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonLimits {
    /// How many downloads may run at once; the rest wait as `queued`.
    pub max_active: usize,
    /// Combined speed cap for all downloads in bytes per second; 0 means unlimited.
    pub max_bytes_per_sec: u64,
}

/// Fields left out keep their current value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetLimitsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_active: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_sec: Option<u64>,
}

/// Defaults for new downloads as the daemon saves them. `None` means the
/// built-in default: the user's Downloads folder, and the engine's connections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    /// Speed cap for each new download; 0 means none.
    #[serde(default)]
    pub max_bytes_per_sec: u64,
}

/// Defaults for new downloads with the built-in values filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadDefaults {
    pub folder: PathBuf,
    pub connections: u32,
    pub max_bytes_per_sec: u64,
}

/// Fields left out keep their current value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDefaultsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Queued,
    Active,
    Pausing,
    Paused,
    Retrying,
    Completed,
    Failed,
    Cancelled,
    /// A video page was found and the user has yet to pick a quality. Nothing
    /// runs, no slot is taken, and it never starts by itself.
    Choosing,
}

/// What a video download shows the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSummary {
    pub title: String,
    /// How long the video runs, when the site says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// The qualities to pick from; only while the download is `choosing`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<VideoChoice>,
    /// Index into `choices` of the row to offer first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<usize>,
    /// The quality that was chosen, once it has been.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<ChosenQuality>,
    /// The size is an estimate: some stream comes in pieces of unknown size.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub size_is_estimate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadSummary {
    pub id: String,
    pub url: String,
    pub destination: PathBuf,
    pub filename: String,
    pub status: DownloadStatus,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Speed cap for this download alone; 0 means none.
    #[serde(default)]
    pub max_bytes_per_sec: u64,
    /// The video behind this download, if it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSummary>,
    /// What the download is doing right now, such as "video 1 of 2".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentProgress {
    pub start: u64,
    pub end: u64,
    pub downloaded_bytes: u64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryTick {
    pub id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: u64,
    pub active_connections: usize,
    /// Byte ranges already on disk, with state "completed".
    pub segments: Vec<SegmentProgress>,
    /// One entry per connection: `downloaded_bytes` is how far it has got from
    /// `start`, and `state` is "active" or "done".
    #[serde(default)]
    pub lanes: Vec<SegmentProgress>,
    /// Why an active download has had no new data for a while, such as
    /// "no data for 42s: server busy (HTTP 503), retrying in 8s".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall: Option<String>,
    /// What the download is doing right now, such as "video 1 of 2".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// For a video sent in pieces: where its pieces are. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pieces: Option<PieceMap>,
}

/// Where a pieces stream's pieces are: `0..written` are in the stream file,
/// `waiting` finished early, and `running` are being fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceMap {
    pub total: u64,
    pub written: u64,
    #[serde(default)]
    pub waiting: Vec<u64>,
    #[serde(default)]
    pub running: Vec<PieceLane>,
}

/// One piece being fetched: its index, and its bytes so far of `total_bytes`
/// when the server said how many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceLane {
    pub index: u64,
    pub downloaded_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadStateChanged {
    pub id: String,
    pub previous_state: DownloadStatus,
    pub new_state: DownloadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The helper programs the daemon can fetch for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolName {
    YtDlp,
    Ffmpeg,
    Deno,
}

/// What the tools manager is doing with a tool right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPhase {
    Idle,
    Checking,
    Downloading,
    Verifying,
    Installing,
}

/// One tool's state, as `tools.status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub tool: ToolName,
    pub installed: Option<String>,
    pub previous: Option<String>,
    pub last_checked_unix: Option<u64>,
    pub phase: ToolPhase,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    /// Why the last attempt failed, in words fit to show the user.
    pub error: Option<String>,
    /// What went wrong underneath `error`, for the command line to show after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `tools.update`: check one tool (installing it if missing), or every installed tool.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolUpdateParams {
    #[serde(default)]
    pub tool: Option<ToolName>,
}

/// `tools.rollback`: go back to the previous version of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRollbackParams {
    pub tool: ToolName,
}

/// Numeric part of a download id such as `dl_12`, for ordering. Other ids sort last.
pub fn download_id_number(id: &str) -> u64 {
    id.strip_prefix("dl_")
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

// --- Encoding & Decoding Helpers ---

/// Encode a value to a compact JSON string terminated with `\n`.
pub fn encode_message<T: Serialize>(val: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(val)?;
    s.push('\n');
    Ok(s)
}

/// Decode a JSON line (ignoring trailing whitespace or newline characters).
pub fn decode_message<T: DeserializeOwned>(raw: &str) -> Result<T, serde_json::Error> {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    serde_json::from_str(trimmed)
}

/// Parse an incoming raw line into a typed `RpcMessage`, verifying JSON-RPC 2.0 conformance.
pub fn parse_rpc_message(raw: &str) -> Result<RpcMessage, RpcError> {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    let val: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return Err(RpcError::parse_error(Some(e.to_string()))),
    };

    let obj = match val.as_object() {
        Some(o) => o,
        None => {
            return Err(RpcError::invalid_request(Some(
                "Payload is not a JSON object".into(),
            )))
        }
    };

    let version = obj.get("jsonrpc").and_then(|v| v.as_str());
    if version != Some(JSONRPC_VERSION) {
        return Err(RpcError::invalid_request(Some(
            "Missing or invalid 'jsonrpc' field, expected '2.0'".into(),
        )));
    }

    let has_id = obj.contains_key("id");
    let has_method = obj.contains_key("method");
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");

    if has_method && has_id {
        // Request
        serde_json::from_value(val)
            .map(RpcMessage::Request)
            .map_err(|e| RpcError::invalid_request(Some(e.to_string())))
    } else if has_method && !has_id {
        // Notification
        serde_json::from_value(val)
            .map(RpcMessage::Notification)
            .map_err(|e| RpcError::invalid_request(Some(e.to_string())))
    } else if has_id && (has_result || has_error) {
        // Response
        serde_json::from_value(val)
            .map(RpcMessage::Response)
            .map_err(|e| RpcError::invalid_request(Some(e.to_string())))
    } else {
        Err(RpcError::invalid_request(Some(
            "Malformed JSON-RPC message structure".into(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn choosing_serializes_as_choosing() {
        assert_eq!(
            serde_json::to_string(&DownloadStatus::Choosing).unwrap(),
            r#""choosing""#
        );
        let back: DownloadStatus = serde_json::from_str(r#""choosing""#).unwrap();
        assert_eq!(back, DownloadStatus::Choosing);
    }

    #[test]
    fn an_add_without_quality_still_parses() {
        let params: AddDownloadParams =
            serde_json::from_str(r#"{"url":"https://example.com/a.bin"}"#).unwrap();
        assert_eq!(params.quality, None);
        let json = serde_json::to_string(&params).unwrap();
        assert!(!json.contains("quality"), "{json}");
    }

    #[test]
    fn a_summary_without_video_or_step_still_parses_and_leaves_them_out() {
        let json = r#"{"id":"dl_1","url":"u","destination":"/d","filename":"f",
            "status":"queued","downloaded_bytes":0,"total_bytes":null,"speed_bps":0}"#;
        let summary: DownloadSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.video, None);
        assert_eq!(summary.step, None);
        let out = serde_json::to_string(&summary).unwrap();
        assert!(!out.contains("video") && !out.contains("step"), "{out}");
    }

    #[test]
    fn a_choose_quality_call_leaves_out_what_it_does_not_set() {
        let params = ChooseQualityParams {
            id: "dl_1".to_string(),
            choice_id: "1080-mp4".to_string(),
            destination: None,
            filename: None,
        };
        let json = serde_json::to_string(&params).unwrap();
        assert_eq!(json, r#"{"id":"dl_1","choice_id":"1080-mp4"}"#);
        let back: ChooseQualityParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back, params);
    }

    #[test]
    fn test_rpc_request_roundtrip_number_id() {
        let req = RpcRequest::new(RpcId::Number(42), "daemon.ping".to_string(), None);
        let encoded = encode_message(&req).unwrap();
        assert!(encoded.ends_with('\n'));
        let parsed: RpcRequest = decode_message(&encoded).unwrap();
        assert_eq!(parsed.jsonrpc, "2.0");
        assert_eq!(parsed.id, RpcId::Number(42));
        assert_eq!(parsed.method, "daemon.ping");
        assert_eq!(parsed.params, None);
    }

    #[test]
    fn test_rpc_request_with_string_id_and_params() {
        let params = AddDownloadParams {
            url: "https://example.com/file.bin".to_string(),
            destination: Some(PathBuf::from("/downloads")),
            filename: Some("file.bin".to_string()),
            connections: Some(8),
            max_bytes_per_sec: None,
            context: None,
            quality: None,
        };
        let req = RpcRequest::new(
            RpcId::String("req_01".to_string()),
            "download.add",
            Some(serde_json::to_value(&params).unwrap()),
        );

        let encoded = encode_message(&req).unwrap();
        let parsed_msg = parse_rpc_message(&encoded).unwrap();
        match parsed_msg {
            RpcMessage::Request(r) => {
                assert_eq!(r.id, RpcId::String("req_01".to_string()));
                assert_eq!(r.method, "download.add");
                let parsed_params: AddDownloadParams =
                    serde_json::from_value(r.params.unwrap()).unwrap();
                assert_eq!(parsed_params.url, "https://example.com/file.bin");
                assert_eq!(parsed_params.connections, Some(8));
            }
            _ => panic!("Expected RpcMessage::Request"),
        }
    }

    #[test]
    fn test_rpc_response_success() {
        let result = PingResult {
            version: "0.1.0".to_string(),
            uptime_secs: 120,
            data_dir: None,
            log_path: None,
        };
        let resp = RpcResponse::success(RpcId::Number(1), serde_json::to_value(&result).unwrap());

        let encoded = encode_message(&resp).unwrap();
        let parsed = parse_rpc_message(&encoded).unwrap();
        match parsed {
            RpcMessage::Response(r) => {
                assert_eq!(r.id, RpcId::Number(1));
                assert!(r.error.is_none());
                let res: PingResult = serde_json::from_value(r.result.unwrap()).unwrap();
                assert_eq!(res.version, "0.1.0");
                assert_eq!(res.uptime_secs, 120);
            }
            _ => panic!("Expected RpcMessage::Response"),
        }
    }

    #[test]
    fn test_rpc_response_error_roundtrip() {
        let err = RpcError::download_not_found("dl_999");
        let resp = RpcResponse::error(RpcId::String("req_err".to_string()), err);

        let encoded = encode_message(&resp).unwrap();
        let parsed = parse_rpc_message(&encoded).unwrap();
        match parsed {
            RpcMessage::Response(r) => {
                assert_eq!(r.id, RpcId::String("req_err".to_string()));
                assert!(r.result.is_none());
                let e = r.error.unwrap();
                assert_eq!(e.code, CODE_DOWNLOAD_NOT_FOUND);
                assert!(e.message.contains("dl_999"));
                assert_eq!(e.data.unwrap()["id"], "dl_999");
            }
            _ => panic!("Expected RpcMessage::Response"),
        }
    }

    #[test]
    fn test_rpc_notification_roundtrip() {
        let tick = TelemetryTick {
            id: "dl_abc".to_string(),
            downloaded_bytes: 1048576,
            total_bytes: Some(10485760),
            speed_bps: 524288,
            active_connections: 4,
            segments: vec![SegmentProgress {
                start: 0,
                end: 5242879,
                downloaded_bytes: 1048576,
                state: "active".to_string(),
            }],
            lanes: Vec::new(),
            stall: None,
            step: None,
            pieces: None,
        };
        let notif =
            RpcNotification::new("telemetry.tick", Some(serde_json::to_value(&tick).unwrap()));
        assert!(!serde_json::to_string(&tick).unwrap().contains("pieces"));

        let encoded = encode_message(&notif).unwrap();
        let parsed = parse_rpc_message(&encoded).unwrap();
        match parsed {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, "telemetry.tick");
                let parsed_tick: TelemetryTick = serde_json::from_value(n.params.unwrap()).unwrap();
                assert_eq!(parsed_tick.downloaded_bytes, 1048576);
                assert_eq!(parsed_tick.active_connections, 4);
                assert_eq!(parsed_tick.segments.len(), 1);
            }
            _ => panic!("Expected RpcMessage::Notification"),
        }
    }

    #[test]
    fn a_piece_map_survives_the_wire() {
        let json = serde_json::json!({
            "id": "dl_1", "downloaded_bytes": 10, "total_bytes": null, "speed_bps": 0,
            "active_connections": 2, "segments": [],
            "pieces": {"total": 9, "written": 3, "waiting": [5],
                       "running": [{"index": 3, "downloaded_bytes": 4, "total_bytes": 8},
                                   {"index": 4, "downloaded_bytes": 1}]}
        });

        let tick: TelemetryTick = serde_json::from_value(json).unwrap();

        let map = tick.pieces.unwrap();
        assert_eq!((map.total, map.written, map.waiting), (9, 3, vec![5]));
        assert_eq!(map.running[0].total_bytes, Some(8));
        assert_eq!(map.running[1].total_bytes, None);
    }

    #[test]
    fn test_parse_rpc_message_invalid_json() {
        let err = parse_rpc_message("this is not json\n").unwrap_err();
        assert_eq!(err.code, CODE_PARSE_ERROR);
    }

    #[test]
    fn test_parse_rpc_message_missing_version() {
        let raw = r#"{"id": 1, "method": "ping"}"#;
        let err = parse_rpc_message(raw).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_REQUEST);
    }

    #[test]
    fn test_all_domain_errors_codes_and_payloads() {
        // Lifecycle
        assert_eq!(
            RpcError::download_not_found("x").code,
            CODE_DOWNLOAD_NOT_FOUND
        );
        assert_eq!(
            RpcError::cannot_pause_completed("x").code,
            CODE_CANNOT_PAUSE_COMPLETED
        );
        assert_eq!(
            RpcError::already_completed("x").code,
            CODE_ALREADY_COMPLETED
        );
        assert_eq!(
            RpcError::operation_in_progress("x", "pause").code,
            CODE_OPERATION_IN_PROGRESS
        );
        assert_eq!(
            RpcError::duplicate_download("http://a", "dl_1").code,
            CODE_DUPLICATE_DOWNLOAD
        );

        // Storage & FS
        let space_err = RpcError::insufficient_disk_space(2000, 1000);
        assert_eq!(space_err.code, CODE_INSUFFICIENT_DISK_SPACE);
        assert_eq!(space_err.data.unwrap()["required_bytes"], 2000);

        assert_eq!(
            RpcError::disk_full_mid_flight("x").code,
            CODE_DISK_FULL_MID_FLIGHT
        );
        assert_eq!(
            RpcError::destination_exists(Path::new("/tmp/foo")).code,
            CODE_DESTINATION_EXISTS
        );
        assert_eq!(
            RpcError::invalid_path(Path::new("/tmp/foo"), "bad char").code,
            CODE_INVALID_PATH
        );
        assert_eq!(
            RpcError::permission_denied(Path::new("/tmp/foo")).code,
            CODE_PERMISSION_DENIED
        );
        assert_eq!(
            RpcError::file_locked(Path::new("/tmp/foo")).code,
            CODE_FILE_LOCKED
        );
        assert_eq!(
            RpcError::corrupt_state_file(Path::new("/tmp/foo"), "bad crc").code,
            CODE_CORRUPT_STATE_FILE
        );

        // Network
        assert_eq!(RpcError::network_error("timeout").code, CODE_NETWORK_ERROR);
        let exp_err = RpcError::link_expired("dl_1", "http://example.com");
        assert_eq!(exp_err.code, CODE_LINK_EXPIRED);
        assert_eq!(exp_err.data.unwrap()["url"], "http://example.com");

        let drift_err = RpcError::file_changed_on_server("dl_1", "etag_1", "etag_2");
        assert_eq!(drift_err.code, CODE_FILE_CHANGED_ON_SERVER);
        assert_eq!(drift_err.data.unwrap()["actual"], "etag_2");

        assert_eq!(
            RpcError::server_lost_range_support("dl_1").code,
            CODE_SERVER_LOST_RANGE_SUPPORT
        );
        assert_eq!(RpcError::rate_limited(Some(60)).code, CODE_RATE_LIMITED);
        assert_eq!(
            RpcError::too_many_redirects("http://loop").code,
            CODE_TOO_MANY_REDIRECTS
        );

        // Daemon
        assert_eq!(
            RpcError::daemon_already_running().code,
            CODE_DAEMON_ALREADY_RUNNING
        );
        assert_eq!(
            RpcError::daemon_shutting_down().code,
            CODE_DAEMON_SHUTTING_DOWN
        );
    }

    #[test]
    fn test_download_status_serialization() {
        let statuses = vec![
            (DownloadStatus::Queued, "\"queued\""),
            (DownloadStatus::Active, "\"active\""),
            (DownloadStatus::Pausing, "\"pausing\""),
            (DownloadStatus::Paused, "\"paused\""),
            (DownloadStatus::Retrying, "\"retrying\""),
            (DownloadStatus::Completed, "\"completed\""),
            (DownloadStatus::Failed, "\"failed\""),
            (DownloadStatus::Cancelled, "\"cancelled\""),
        ];

        for (status, expected_json) in statuses {
            let serialized = serde_json::to_string(&status).unwrap();
            assert_eq!(serialized, expected_json);
            let deserialized: DownloadStatus = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, status);
        }
    }

    #[test]
    fn test_decode_message_handles_crlf_and_lf() {
        let req = RpcRequest::new(RpcId::Number(1), "daemon.ping", None);
        let json_str = serde_json::to_string(&req).unwrap();

        let lf = format!("{}\n", json_str);
        let crlf = format!("{}\r\n", json_str);

        let parsed_lf: RpcRequest = decode_message(&lf).unwrap();
        let parsed_crlf: RpcRequest = decode_message(&crlf).unwrap();

        assert_eq!(parsed_lf, req);
        assert_eq!(parsed_crlf, req);
    }

    #[test]
    fn defaults_left_out_mean_unchanged_or_built_in() {
        let unchanged = SetDefaultsParams::default();
        assert_eq!(serde_json::to_string(&unchanged).unwrap(), "{}");
        let saved: SavedDefaults = serde_json::from_str("{}").unwrap();
        assert_eq!(saved, SavedDefaults::default());
        assert_eq!(saved.folder, None);
        assert_eq!(saved.connections, None);
        assert_eq!(saved.max_bytes_per_sec, 0);
    }

    #[test]
    fn an_old_ping_reply_without_paths_still_decodes() {
        let ping: PingResult =
            serde_json::from_str(r#"{"version":"0.1.0","uptime_secs":5}"#).unwrap();
        assert_eq!(ping.data_dir, None);
        assert_eq!(ping.log_path, None);
        let encoded = serde_json::to_string(&ping).unwrap();
        assert!(!encoded.contains("log_path"), "{encoded}");
    }

    #[test]
    fn tool_requests_and_replies_use_the_documented_names() {
        assert_eq!(
            serde_json::to_string(&ToolName::YtDlp).unwrap(),
            "\"yt-dlp\""
        );
        assert_eq!(
            serde_json::to_string(&ToolPhase::Downloading).unwrap(),
            "\"downloading\""
        );
        let update: ToolUpdateParams = serde_json::from_str("{}").unwrap();
        assert_eq!(update.tool, None);
        let update: ToolUpdateParams = serde_json::from_str(r#"{"tool":"deno"}"#).unwrap();
        assert_eq!(update.tool, Some(ToolName::Deno));
        assert!(serde_json::from_str::<ToolRollbackParams>("{}").is_err());

        let info = ToolInfo {
            tool: ToolName::Ffmpeg,
            installed: Some("8.1.3".into()),
            previous: None,
            last_checked_unix: Some(1_794_839_520),
            phase: ToolPhase::Idle,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
            detail: Some("dns error".into()),
        };
        let back: ToolInfo = serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(back, info);
        assert_eq!(RpcError::tool_failed("x").code, CODE_TOOL_FAILED);
    }
}
