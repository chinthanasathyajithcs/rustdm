//! The host's main loop: read a request frame, ask `rdmd`, write a reply frame.
//! Only `ping` and `add` can reach the daemon; nothing else is forwarded.

use crate::daemon::launch::{daemon_exe_path, spawn_detached};
use crate::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use crate::ipc::protocol::{
    AddedBy, ChooseQualityParams, DownloadStatus, DownloadSummary, CODE_DOWNLOAD_NOT_FOUND,
    CODE_DUPLICATE_DOWNLOAD, CODE_INVALID_PARAMS,
};
use crate::ipc::transport::IpcEndpoint;
use crate::nmhost::frame::{read_frame, write_frame, FrameError};
use crate::nmhost::message::{
    add_params, check_download_id, HostErrorKind, HostReply, HostRequest, PROTOCOL_VERSION,
};
use crate::request_context::RequestContext;
use serde_json::{json, Map, Value};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// How long one request may take, including starting the daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `add` waits for a daemon it started to begin listening.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Where this machine's named pipes live.
const LOCAL_PIPE_PREFIX: &str = r"\\.\pipe\";

/// A daemon endpoint given by the environment (tests only). A Windows pipe
/// name must be on this machine: `\\server\pipe\x` would open a pipe on
/// another computer.
pub fn endpoint_from_override(raw: &str) -> Result<IpcEndpoint, String> {
    require_local(IpcEndpoint::from_arg(raw))
}

fn require_local(endpoint: IpcEndpoint) -> Result<IpcEndpoint, String> {
    match &endpoint {
        IpcEndpoint::NamedPipe(name) if !name.starts_with(LOCAL_PIPE_PREFIX) => {
            Err("endpoint must be a local pipe".to_string())
        }
        _ => Ok(endpoint),
    }
}

/// Serve requests until the browser closes the input. Starts `rdmd` from the
/// folder this program is in when an `add` finds none running.
pub async fn run_host<R, W>(
    reader: &mut R,
    writer: &mut W,
    endpoint: &IpcEndpoint,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_host_with(reader, writer, endpoint, || {
        spawn_detached(&daemon_exe_path()?, endpoint)
    })
    .await
}

/// `run_host` with the way to start the daemon supplied by the caller.
pub async fn run_host_with<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    endpoint: &IpcEndpoint,
    spawn: S,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: Fn() -> io::Result<()>,
{
    run_host_timed(reader, writer, endpoint, spawn, REQUEST_TIMEOUT).await
}

/// `run_host_with` with the per-request time limit supplied too (tests use a short one).
pub async fn run_host_timed<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    endpoint: &IpcEndpoint,
    spawn: S,
    request_timeout: Duration,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: Fn() -> io::Result<()>,
{
    loop {
        let reply = match read_frame(reader).await {
            Ok(None) => return Ok(()),
            Ok(Some(bytes)) => answer(&bytes, endpoint, &spawn, request_timeout).await,
            Err(FrameError::TooLarge(len)) => {
                // The oversized body is still unread, so the stream cannot be
                // trusted past this point: say so and stop.
                let reply = HostReply::failure(
                    None,
                    HostErrorKind::TooLarge,
                    None,
                    format!("message of {len} bytes is too large"),
                );
                send(writer, &reply).await?;
                return Ok(());
            }
            Err(FrameError::Truncated) => {
                tracing::warn!("the browser closed the input in the middle of a message");
                return Ok(());
            }
            Err(FrameError::Io(e)) => return Err(e),
        };
        send(writer, &reply).await?;
    }
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, reply: &HostReply) -> io::Result<()> {
    match write_frame(writer, &reply.to_bytes()).await {
        Ok(()) => Ok(()),
        Err(FrameError::Io(e)) => Err(e),
        Err(other) => Err(io::Error::other(other)),
    }
}

async fn answer<S: Fn() -> io::Result<()>>(
    bytes: &[u8],
    endpoint: &IpcEndpoint,
    spawn: &S,
    request_timeout: Duration,
) -> HostReply {
    let request = match HostRequest::parse(bytes) {
        Ok(request) => request,
        Err((id, message)) => {
            return HostReply::failure(id, HostErrorKind::Invalid, None, message);
        }
    };
    let id = request.id();
    match tokio::time::timeout(request_timeout, handle(request, endpoint, spawn)).await {
        Ok(reply) => reply,
        // Giving up does not undo an `add` the daemon is already acting on: it
        // may still finish after this reply, and the browser then fetches the
        // file too. The daemon's duplicate check is what limits the damage.
        Err(_) => HostReply::failure(
            Some(id),
            HostErrorKind::Timeout,
            None,
            "rdmd did not answer in time",
        ),
    }
}

async fn handle<S: Fn() -> io::Result<()>>(
    request: HostRequest,
    endpoint: &IpcEndpoint,
    spawn: &S,
) -> HostReply {
    match request {
        HostRequest::Ping { id } => ping(id, endpoint).await,
        HostRequest::Add {
            id,
            url,
            filename,
            context,
        } => {
            add(
                id,
                &url,
                filename.as_deref(),
                context.as_ref(),
                endpoint,
                spawn,
            )
            .await
        }
        HostRequest::Status { id, download_id } => status(id, &download_id, endpoint).await,
        HostRequest::ChooseQuality {
            id,
            download_id,
            choice_id,
        } => choose_quality(id, &download_id, &choice_id, endpoint).await,
    }
}

/// Report the host and daemon versions. Never starts the daemon.
async fn ping(id: u64, endpoint: &IpcEndpoint) -> HostReply {
    let version = match DaemonClient::connect(endpoint).await {
        Ok(mut client) => client.ping().await.ok().map(|ping| ping.version),
        Err(_) => None,
    };
    HostReply::success(
        id,
        json!({
            "host_version": crate::VERSION,
            "protocol": PROTOCOL_VERSION,
            "daemon": { "running": version.is_some(), "version": version },
        }),
    )
}

async fn add<S: Fn() -> io::Result<()>>(
    id: u64,
    url: &str,
    filename: Option<&str>,
    context: Option<&RequestContext>,
    endpoint: &IpcEndpoint,
    spawn: &S,
) -> HostReply {
    let outcome = async {
        let mut client = connect_or_spawn(endpoint, spawn, SPAWN_TIMEOUT).await?;
        client.add(add_params(url, filename, context)).await
    }
    .await;
    match outcome {
        Ok(summary) => HostReply::success(
            id,
            json!({ "download_id": summary.id, "filename": summary.filename }),
        ),
        Err(e) => {
            let reply = failure_for(id, e);
            log_failure(url, &reply);
            reply
        }
    }
}

/// The one answer for a download that is missing or was not added by the
/// browser, so the browser cannot tell other downloads exist.
const NOT_FOUND: &str = "rustdm has no such download from this browser";

/// Shown instead of a failure message that names a link or a path on this
/// computer: the browser must not learn either (spec 1d, 4.3).
const HIDDEN_ERROR: &str = "The download failed. rustdm shows the details.";

/// How a download the browser added is doing. Never starts the daemon.
async fn status(id: u64, download_id: &str, endpoint: &IpcEndpoint) -> HostReply {
    let mut client = match DaemonClient::connect(endpoint).await {
        Ok(client) => client,
        Err(e) => return failure_for(id, e),
    };
    match browser_download(&mut client, download_id).await {
        Ok(summary) => HostReply::success(id, trimmed_status(&summary)),
        Err(reply) => reply.with_id(id),
    }
}

/// Pick the quality of a download the browser added. Never starts the daemon.
async fn choose_quality(
    id: u64,
    download_id: &str,
    choice_id: &str,
    endpoint: &IpcEndpoint,
) -> HostReply {
    let mut client = match DaemonClient::connect(endpoint).await {
        Ok(client) => client,
        Err(e) => return failure_for(id, e),
    };
    if let Err(reply) = browser_download(&mut client, download_id).await {
        return reply.with_id(id);
    }
    let params = ChooseQualityParams {
        id: download_id.to_string(),
        choice_id: choice_id.to_string(),
        destination: None,
        filename: None,
    };
    match client.choose_quality(params).await {
        Ok(summary) => HostReply::success(id, trimmed_status(&summary)),
        // Not waiting for a quality any more, or no such choice: the daemon's
        // words say which (spec 1d, 4.2).
        Err(ClientError::Rpc(rpc)) => HostReply::failure(
            Some(id),
            HostErrorKind::Rejected,
            Some(rpc.code),
            rpc.message,
        ),
        Err(e) => failure_for(id, e),
    }
}

/// The download, when it is there and the browser added it; else the reply
/// to send (with no id yet).
async fn browser_download(
    client: &mut DaemonClient,
    download_id: &str,
) -> Result<DownloadSummary, HostReply> {
    let not_found = || HostReply::failure(None, HostErrorKind::NotFound, None, NOT_FOUND);
    match client.status(download_id).await {
        Ok(summary) if summary.added_by == Some(AddedBy::Browser) => Ok(summary),
        Ok(_) => Err(not_found()),
        Err(ClientError::Rpc(rpc)) if rpc.code == CODE_DOWNLOAD_NOT_FOUND => Err(not_found()),
        Err(e) => Err(failure_for(0, e)),
    }
}

/// What the browser may know of a download: no folder, path, link, cookie or
/// stream, and nothing about any other download.
fn trimmed_status(summary: &DownloadSummary) -> Value {
    let mut out = Map::new();
    out.insert("download_id".into(), json!(summary.id));
    out.insert("status".into(), json!(summary.status));
    out.insert("filename".into(), json!(summary.filename));
    if let Some(total) = summary.total_bytes.filter(|&t| t > 0) {
        out.insert(
            "progress".into(),
            json!(summary.downloaded_bytes as f64 / total as f64),
        );
    }
    if let Some(video) = &summary.video {
        let choices: Vec<Value> = video
            .choices
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "label": c.label,
                    "container": c.container.label(),
                    "height": c.height,
                    "size_bytes": c.size_bytes,
                    "size_is_estimate": c.size_is_estimate,
                })
            })
            .collect();
        let mut v = Map::new();
        v.insert("title".into(), json!(video.title));
        if let Some(secs) = video.duration_secs {
            v.insert("duration_secs".into(), json!(secs));
        }
        v.insert("choices".into(), Value::Array(choices));
        if let Some(default) = video.default {
            v.insert("default".into(), json!(default));
        }
        if let Some(chosen) = &video.chosen {
            v.insert(
                "chosen".into(),
                json!({"label": chosen.label, "container": chosen.container.label()}),
            );
        }
        out.insert("video".into(), Value::Object(v));
    }
    if summary.status == DownloadStatus::Failed {
        if let Some(error) = &summary.error_message {
            out.insert("error".into(), json!(safe_error(error)));
        }
    }
    Value::Object(out)
}

/// `message`, unless it names a link or a path on this computer.
fn safe_error(message: &str) -> &str {
    let names_a_place = message.contains("://")
        || message.split_whitespace().any(|word| {
            let word = word.trim_start_matches(['(', '"', '\'']);
            word.starts_with('/')
                || word.starts_with("~/")
                || word.starts_with("\\\\")
                || word.as_bytes().get(1..3) == Some(b":\\")
        });
    if names_a_place {
        HIDDEN_ERROR
    } else {
        message
    }
}

/// Log a refused hand-off: the host name, the error kind and the daemon's code
/// only. Messages can carry the whole URL (which can hold tokens), and cookies
/// are never logged. A duplicate is routine, so it is not a warning.
fn log_failure(url: &str, reply: &HostReply) {
    let Some(error) = &reply.error else { return };
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    if error.kind == HostErrorKind::Duplicate {
        tracing::debug!(%host, "download already known to rdmd");
    } else {
        tracing::warn!(%host, kind = ?error.kind, code = ?error.code, "could not hand the download to rdmd");
    }
}

fn failure_for(id: u64, error: ClientError) -> HostReply {
    let id = Some(id);
    match error {
        ClientError::NotRunning => {
            HostReply::failure(id, HostErrorKind::NotRunning, None, error.to_string())
        }
        ClientError::Spawn(_) | ClientError::SpawnTimeout(_) => {
            HostReply::failure(id, HostErrorKind::SpawnFailed, None, error.to_string())
        }
        ClientError::Rpc(rpc) => {
            let kind = match rpc.code {
                CODE_DUPLICATE_DOWNLOAD => HostErrorKind::Duplicate,
                CODE_INVALID_PARAMS => HostErrorKind::Invalid,
                _ => HostErrorKind::Rejected,
            };
            let existing_id = (kind == HostErrorKind::Duplicate)
                .then(|| rpc.data.as_ref()?.get("existing_id")?.as_str())
                .flatten()
                .filter(|existing| check_download_id(existing).is_ok())
                .map(str::to_string);
            let mut reply = HostReply::failure(id, kind, Some(rpc.code), rpc.message);
            if let Some(error) = reply.error.as_mut() {
                error.existing_id = existing_id;
            }
            reply
        }
        other => HostReply::failure(id, HostErrorKind::Rejected, None, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{AddedBy, RpcError, VideoSummary};
    use crate::video::{ChosenQuality, Container, StreamKind, StreamPick, VideoChoice};
    use serde_json::Value;

    async fn replies_to(frames: &[&[u8]]) -> Vec<Value> {
        let mut input = Vec::new();
        for frame in frames {
            write_frame(&mut input, frame).await.unwrap();
        }
        let mut output = Vec::new();
        let endpoint = IpcEndpoint::test_endpoint("relay_unit");
        run_host_with(&mut &input[..], &mut output, &endpoint, || {
            Err(io::Error::other("no daemon in unit tests"))
        })
        .await
        .unwrap();
        let mut replies = Vec::new();
        let mut rest = &output[..];
        while let Some(body) = read_frame(&mut rest).await.unwrap() {
            replies.push(serde_json::from_slice(&body).unwrap());
        }
        replies
    }

    #[tokio::test]
    async fn ping_works_without_a_daemon_and_does_not_start_one() {
        let replies = replies_to(&[br#"{"type":"ping","id":1}"#]).await;
        assert_eq!(replies.len(), 1);
        let reply = &replies[0];
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["protocol"], PROTOCOL_VERSION);
        assert_eq!(reply["result"]["daemon"]["running"], false);
    }

    #[tokio::test]
    async fn bad_requests_get_an_invalid_reply_and_the_loop_continues() {
        let replies = replies_to(&[
            br#"{"type":"shutdown","id":4}"#,
            b"not json",
            br#"{"type":"add","id":5,"url":"file:///etc/passwd"}"#,
            br#"{"type":"ping","id":6}"#,
        ])
        .await;
        assert_eq!(replies.len(), 4);
        assert_eq!(replies[0]["id"], 4);
        assert_eq!(replies[0]["error"]["kind"], "invalid");
        assert_eq!(replies[1]["id"], Value::Null);
        assert_eq!(replies[2]["id"], 5);
        assert_eq!(replies[2]["error"]["kind"], "invalid");
        assert_eq!(replies[3]["ok"], true);
    }

    #[tokio::test]
    async fn an_add_with_no_daemon_and_no_way_to_start_one_reports_spawn_failed() {
        let replies =
            replies_to(&[br#"{"type":"add","id":8,"url":"https://example.com/x"}"#]).await;
        assert_eq!(replies[0]["id"], 8);
        assert_eq!(replies[0]["error"]["kind"], "spawn_failed");
    }

    #[tokio::test]
    async fn a_daemon_that_never_starts_listening_ends_in_a_timeout_reply() {
        let mut input = Vec::new();
        write_frame(
            &mut input,
            br#"{"type":"add","id":4,"url":"https://example.com/x"}"#,
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        let endpoint = IpcEndpoint::test_endpoint("relay_slow");
        let started = std::time::Instant::now();
        // The "daemon" starts fine but never listens, so the add keeps waiting.
        run_host_timed(
            &mut &input[..],
            &mut output,
            &endpoint,
            || Ok(()),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        let body = read_frame(&mut &output[..]).await.unwrap().unwrap();
        let reply: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["kind"], "timeout");
    }

    #[test]
    fn only_local_endpoints_are_accepted_from_the_environment() {
        let pipe = |name: &str| IpcEndpoint::NamedPipe(name.to_string());
        assert!(require_local(pipe(r"\\.\pipe\rustdm_test")).is_ok());
        assert!(require_local(pipe(r"\\server\pipe\rustdm")).is_err());
        assert!(require_local(pipe(r"\\?\UNC\server\pipe\x")).is_err());
        assert!(require_local(pipe("rustdm")).is_err());
        assert!(require_local(IpcEndpoint::UnixSocket("/tmp/a.sock".into())).is_ok());
        #[cfg(windows)]
        {
            assert!(endpoint_from_override("rustdm_test").is_ok());
            assert!(endpoint_from_override(r"\\server\pipe\rustdm").is_err());
        }
    }

    #[tokio::test]
    async fn an_oversized_frame_gets_one_reply_and_ends_the_loop() {
        let mut input = ((crate::nmhost::frame::MAX_FRAME_BYTES + 1) as u32)
            .to_ne_bytes()
            .to_vec();
        write_frame(&mut input, br#"{"type":"ping","id":1}"#)
            .await
            .unwrap();
        let mut output = Vec::new();
        let endpoint = IpcEndpoint::test_endpoint("relay_unit");
        run_host_with(&mut &input[..], &mut output, &endpoint, || Ok(()))
            .await
            .unwrap();
        let mut rest = &output[..];
        let body = read_frame(&mut rest).await.unwrap().unwrap();
        let reply: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reply["error"]["kind"], "too_large");
        assert!(read_frame(&mut rest).await.unwrap().is_none());
    }

    #[test]
    fn daemon_refusals_map_to_their_kinds() {
        let duplicate = failure_for(
            2,
            ClientError::Rpc(RpcError::duplicate_download(
                "https://example.com/x",
                "dl_1",
            )),
        );
        assert_eq!(
            duplicate.error.as_ref().unwrap().kind,
            HostErrorKind::Duplicate
        );
        assert_eq!(
            duplicate.error.as_ref().unwrap().code,
            Some(CODE_DUPLICATE_DOWNLOAD)
        );

        let invalid = failure_for(2, ClientError::Rpc(RpcError::invalid_params("bad")));
        assert_eq!(invalid.error.unwrap().kind, HostErrorKind::Invalid);

        let other = failure_for(2, ClientError::Rpc(RpcError::daemon_shutting_down()));
        assert_eq!(other.error.unwrap().kind, HostErrorKind::Rejected);

        let gone = failure_for(2, ClientError::NotRunning);
        assert_eq!(gone.error.unwrap().kind, HostErrorKind::NotRunning);
    }

    fn summary(status: DownloadStatus) -> DownloadSummary {
        DownloadSummary {
            id: "dl_1".to_string(),
            url: "https://e.com/watch?v=x".to_string(),
            destination: std::path::PathBuf::from("/home/u/Downloads/x.mp4"),
            filename: "Angel One.mp4".to_string(),
            status,
            downloaded_bytes: 50,
            total_bytes: Some(200),
            speed_bps: 0,
            eta_secs: None,
            error_message: None,
            max_bytes_per_sec: 0,
            video: None,
            step: None,
            added_by: Some(AddedBy::Browser),
        }
    }

    fn choosing_video() -> VideoSummary {
        VideoSummary {
            title: "Angel One".to_string(),
            duration_secs: Some(60),
            choices: vec![VideoChoice {
                id: "1080-mp4".to_string(),
                label: "1080p".to_string(),
                container: Container::Mp4,
                height: Some(1080),
                size_bytes: Some(40_000_000),
                size_is_estimate: true,
                streams: vec![StreamPick {
                    format_id: "fmt-137-secret".to_string(),
                    kind: StreamKind::Video,
                    ext: "mp4".to_string(),
                }],
            }],
            default: Some(0),
            chosen: None,
            size_is_estimate: true,
        }
    }

    #[test]
    fn status_of_a_browser_download_is_trimmed() {
        let mut row = summary(DownloadStatus::Choosing);
        row.video = Some(choosing_video());

        let reply = trimmed_status(&row);

        assert_eq!(reply["download_id"], "dl_1");
        assert_eq!(reply["status"], "choosing");
        assert_eq!(reply["filename"], "Angel One.mp4");
        assert_eq!(reply["video"]["title"], "Angel One");
        assert_eq!(reply["video"]["duration_secs"], 60);
        assert_eq!(reply["video"]["default"], 0);
        let choice = &reply["video"]["choices"][0];
        assert_eq!(choice["id"], "1080-mp4");
        assert_eq!(choice["label"], "1080p");
        assert_eq!(choice["container"], "MP4");
        assert_eq!(choice["height"], 1080);
        assert_eq!(choice["size_bytes"], 40_000_000);
        assert_eq!(choice["size_is_estimate"], true);
        let text = reply.to_string();
        for leak in [
            "http",
            "/home/",
            "fmt-137-secret",
            "streams",
            "destination",
            "\"url\"",
        ] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
    }

    #[test]
    fn a_chosen_quality_and_an_error_are_reported() {
        let mut row = summary(DownloadStatus::Failed);
        let mut video = choosing_video();
        video.choices.clear();
        video.chosen = Some(ChosenQuality {
            label: "720p".to_string(),
            container: Container::Webm,
            choice_id: "720-webm".to_string(),
        });
        row.video = Some(video);
        row.error_message = Some("The site refused the video.".to_string());

        let reply = trimmed_status(&row);

        assert_eq!(
            reply["video"]["chosen"],
            json!({"label": "720p", "container": "WebM"})
        );
        assert_eq!(reply["video"]["choices"], json!([]));
        assert_eq!(reply["error"], "The site refused the video.");
    }

    #[test]
    fn progress_is_the_done_share_when_known() {
        assert_eq!(
            trimmed_status(&summary(DownloadStatus::Active))["progress"],
            0.25
        );
    }

    #[test]
    fn progress_is_left_out_when_the_total_is_unknown() {
        for total in [None, Some(0)] {
            let row = DownloadSummary {
                total_bytes: total,
                ..summary(DownloadStatus::Active)
            };
            let reply = trimmed_status(&row);
            assert!(reply.get("progress").is_none(), "{total:?}");
            assert!(reply.get("video").is_none());
            assert!(reply.get("error").is_none());
        }
    }

    #[test]
    fn a_duplicate_carries_the_existing_id() {
        let reply = failure_for(
            3,
            ClientError::Rpc(RpcError::duplicate_download("https://e.com/x", "dl_7")),
        );
        let value: Value = serde_json::from_slice(&reply.to_bytes()).unwrap();
        assert_eq!(value["error"]["kind"], "duplicate");
        assert_eq!(value["error"]["existing_id"], "dl_7");
    }

    #[test]
    fn a_malformed_existing_id_is_dropped() {
        for data in [
            Some(json!({"existing_id": "dl 7/.."})),
            Some(json!({"existing_id": 7})),
            None,
        ] {
            let rpc = RpcError {
                code: CODE_DUPLICATE_DOWNLOAD,
                message: "dup".to_string(),
                data: data.clone(),
            };
            let value: Value =
                serde_json::from_slice(&failure_for(3, ClientError::Rpc(rpc)).to_bytes()).unwrap();
            assert_eq!(value["error"]["kind"], "duplicate", "{data:?}");
            assert!(value["error"].get("existing_id").is_none(), "{data:?}");
        }
    }

    #[test]
    fn other_failures_carry_no_existing_id() {
        let value: Value =
            serde_json::from_slice(&failure_for(3, ClientError::NotRunning).to_bytes()).unwrap();
        assert!(value["error"].get("existing_id").is_none());
    }

    #[test]
    fn an_error_never_carries_a_link_or_a_path() {
        for message in [
            "error sending request for url (https://cdn.example/seg1.ts?sig=abc)",
            "Not enough disk space: C:\\Users\\amal\\Downloads\\x.mp4 needs 2 GiB",
            "could not write /home/amal/Downloads/x.mp4",
            "cannot reach \\\\nas\\share\\x.mp4",
        ] {
            let row = DownloadSummary {
                error_message: Some(message.to_string()),
                ..summary(DownloadStatus::Failed)
            };
            let reply = trimmed_status(&row);
            let text = reply["error"].as_str().unwrap();
            for leak in ["://", "Users", "/home/", "nas", "sig="] {
                assert!(!text.contains(leak), "{leak} in {text:?}");
            }
            assert_eq!(text, HIDDEN_ERROR);
        }
    }

    #[test]
    fn a_plain_failure_message_is_kept() {
        let row = DownloadSummary {
            error_message: Some("The site says you need to sign in to watch it.".to_string()),
            ..summary(DownloadStatus::Failed)
        };
        assert_eq!(
            trimmed_status(&row)["error"],
            "The site says you need to sign in to watch it."
        );
    }

    #[test]
    fn only_a_failed_download_reports_its_error() {
        let row = DownloadSummary {
            error_message: Some("Connection reset; trying again".to_string()),
            ..summary(DownloadStatus::Retrying)
        };
        assert!(trimmed_status(&row).get("error").is_none());
    }
}
