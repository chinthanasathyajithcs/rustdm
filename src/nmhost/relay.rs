//! The host's main loop: read a request frame, ask `rdmd`, write a reply frame.
//! Only `ping` and `add` can reach the daemon; nothing else is forwarded.

use crate::daemon::launch::{daemon_exe_path, spawn_detached};
use crate::ipc::client::{connect_or_spawn, ClientError, DaemonClient};
use crate::ipc::protocol::{DownloadSummary, CODE_DUPLICATE_DOWNLOAD, CODE_INVALID_PARAMS};
use crate::ipc::transport::IpcEndpoint;
use crate::nmhost::frame::{read_frame, write_frame, FrameError};
use crate::nmhost::message::{
    add_params_json, HostErrorKind, HostReply, HostRequest, PROTOCOL_VERSION,
};
use serde_json::{json, Map, Value};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// How long one request may take, including starting the daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `add` waits for a daemon it started to begin listening.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

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
    loop {
        let reply = match read_frame(reader).await {
            Ok(None) => return Ok(()),
            Ok(Some(bytes)) => answer(&bytes, endpoint, &spawn).await,
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
) -> HostReply {
    let request = match HostRequest::parse(bytes) {
        Ok(request) => request,
        Err((id, message)) => {
            return HostReply::failure(id, HostErrorKind::Invalid, None, message);
        }
    };
    let id = request.id();
    match tokio::time::timeout(REQUEST_TIMEOUT, handle(request, endpoint, spawn)).await {
        Ok(reply) => reply,
        Err(_) => HostReply::failure(
            Some(id),
            HostErrorKind::Rejected,
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
    context: Option<&Map<String, Value>>,
    endpoint: &IpcEndpoint,
    spawn: &S,
) -> HostReply {
    let outcome = async {
        let mut client = connect_or_spawn(endpoint, || spawn(), SPAWN_TIMEOUT).await?;
        client
            .call::<DownloadSummary>(
                "download.add",
                Some(add_params_json(url, filename, context)),
            )
            .await
    }
    .await;
    match outcome {
        Ok(summary) => HostReply::success(
            id,
            json!({ "download_id": summary.id, "filename": summary.filename }),
        ),
        Err(e) => {
            // The host name only: URLs can carry tokens, and cookies are never logged.
            tracing::warn!(
                host = url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .as_deref()
                    .unwrap_or("?"),
                "could not hand the download to rdmd: {e}"
            );
            failure_for(id, e)
        }
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
