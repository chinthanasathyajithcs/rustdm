//! The host's main loop: read a request frame, ask `rdmd`, write a reply frame.
//! Only `ping` and `add` can reach the daemon; nothing else is forwarded.

use crate::daemon::launch::{daemon_exe_path, spawn_detached};
use crate::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use crate::ipc::protocol::{CODE_DUPLICATE_DOWNLOAD, CODE_INVALID_PARAMS};
use crate::ipc::transport::IpcEndpoint;
use crate::nmhost::frame::{read_frame, write_frame, FrameError};
use crate::nmhost::message::{add_params, HostErrorKind, HostReply, HostRequest, PROTOCOL_VERSION};
use crate::request_context::RequestContext;
use serde_json::json;
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
            HostReply::failure(id, kind, Some(rpc.code), rpc.message)
        }
        other => HostReply::failure(id, HostErrorKind::Rejected, None, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::RpcError;
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
}
