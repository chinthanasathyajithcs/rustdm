use rustdm::ipc::protocol::{
    PingResult, RpcId, RpcMessage, RpcNotification, RpcRequest, RpcResponse, TelemetryTick,
};
use rustdm::ipc::transport::{IpcConnection, IpcEndpoint, IpcListener};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_test_endpoint() -> IpcEndpoint {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::test_endpoint(&format!("test_{}", id))
}

#[tokio::test]
async fn test_client_server_ping_pong() {
    let endpoint = next_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).await.expect("bind listener");

    let server_task = tokio::spawn(async move {
        let mut server_conn = listener.accept().await.expect("accept client");
        let msg = server_conn
            .recv()
            .await
            .expect("recv msg")
            .expect("some msg");
        match msg {
            RpcMessage::Request(req) => {
                assert_eq!(req.method, "daemon.ping");
                let result = PingResult {
                    version: "0.1.0".to_string(),
                    uptime_secs: 42,
                    data_dir: None,
                    log_path: None,
                };
                let resp = RpcResponse::success(req.id, serde_json::to_value(&result).unwrap());
                server_conn.send(&resp).await.expect("send response");
            }
            other => panic!("expected Request, got {:?}", other),
        }
    });

    let mut client_conn = IpcConnection::connect(&endpoint)
        .await
        .expect("client connect");
    let req = RpcRequest::new(RpcId::Number(1), "daemon.ping", None);
    client_conn.send(&req).await.expect("client send");

    let resp_msg = client_conn
        .recv()
        .await
        .expect("client recv")
        .expect("some msg");
    match resp_msg {
        RpcMessage::Response(resp) => {
            assert_eq!(resp.id, RpcId::Number(1));
            let ping: PingResult = serde_json::from_value(resp.result.unwrap()).unwrap();
            assert_eq!(ping.version, "0.1.0");
            assert_eq!(ping.uptime_secs, 42);
        }
        other => panic!("expected Response, got {:?}", other),
    }

    server_task.await.expect("server task join");
}

#[tokio::test]
async fn test_split_full_duplex_streaming() {
    let endpoint = next_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).await.expect("bind listener");

    let server_task = tokio::spawn(async move {
        let server_conn = listener.accept().await.expect("accept client");
        let (mut sender, _receiver) = server_conn.into_split();

        for i in 1..=3 {
            let tick = TelemetryTick {
                id: "dl_stream".to_string(),
                downloaded_bytes: i * 1024,
                total_bytes: Some(10240),
                speed_bps: 1024,
                active_connections: 2,
                segments: Vec::new(),
                lanes: Vec::new(),
                stall: None,
                step: None,
                pieces: None,
            };
            let notif =
                RpcNotification::new("telemetry.tick", Some(serde_json::to_value(&tick).unwrap()));
            sender.send(&notif).await.expect("send tick");
        }
    });

    let client_conn = IpcConnection::connect(&endpoint)
        .await
        .expect("client connect");
    let (_sender, mut receiver) = client_conn.into_split();

    for i in 1..=3 {
        let msg = receiver.recv().await.expect("recv msg").expect("some tick");
        match msg {
            RpcMessage::Notification(n) => {
                assert_eq!(n.method, "telemetry.tick");
                let tick: TelemetryTick = serde_json::from_value(n.params.unwrap()).unwrap();
                assert_eq!(tick.downloaded_bytes, i * 1024);
            }
            other => panic!("expected notification, got {:?}", other),
        }
    }

    server_task.await.expect("server task join");
}

#[tokio::test]
async fn test_clean_client_disconnect_eof() {
    let endpoint = next_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).await.expect("bind listener");

    let server_task = tokio::spawn(async move {
        let mut server_conn = listener.accept().await.expect("accept client");
        let msg = server_conn.recv().await.expect("recv before close");
        assert!(msg.is_some());

        // Now client closes connection, server should receive Ok(None)
        let eof = server_conn.recv().await.expect("recv eof");
        assert!(eof.is_none(), "expected EOF on disconnect");
    });

    let mut client_conn = IpcConnection::connect(&endpoint)
        .await
        .expect("client connect");
    let req = RpcRequest::new(RpcId::Number(99), "daemon.ping", None);
    client_conn.send(&req).await.expect("send req");
    drop(client_conn); // client closes socket

    server_task.await.expect("server task join");
}

#[tokio::test]
async fn test_multiple_concurrent_clients() {
    let endpoint = next_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).await.expect("bind listener");

    let server_task = tokio::spawn(async move {
        for _ in 0..3 {
            let mut conn = listener.accept().await.expect("accept");
            tokio::spawn(async move {
                let msg = conn.recv().await.unwrap().unwrap();
                if let RpcMessage::Request(req) = msg {
                    let resp = RpcResponse::success(req.id, serde_json::json!({ "ack": true }));
                    conn.send(&resp).await.unwrap();
                }
            });
        }
    });

    let mut handles = Vec::new();
    for i in 0..3 {
        let ep = endpoint.clone();
        handles.push(tokio::spawn(async move {
            let mut client = IpcConnection::connect(&ep).await.expect("connect");
            let req = RpcRequest::new(RpcId::Number(i), "test.ping", None);
            client.send(&req).await.expect("send");
            let resp = client.recv().await.unwrap().unwrap();
            match resp {
                RpcMessage::Response(r) => assert_eq!(r.id, RpcId::Number(i)),
                _ => panic!("expected response"),
            }
        }));
    }

    for h in handles {
        h.await.expect("client handle");
    }
    server_task.await.expect("server task");
}

#[tokio::test]
async fn test_stale_socket_cleanup_and_active_detection() {
    let endpoint = next_test_endpoint();

    // 1. First server binds
    let listener = IpcListener::bind(&endpoint).await.expect("bind initial");

    // 2. Second bind while first is active should fail with AddressInUse
    let second_bind = IpcListener::bind(&endpoint).await;
    assert!(second_bind.is_err(), "should fail to bind active socket");

    // Drop the active listener
    drop(listener);

    // 3. If a stale socket file was left without a running server (simulate dead file on Unix)
    #[cfg(unix)]
    {
        if let Some(path) = endpoint.as_path() {
            // Create a fake dummy file at the socket path
            std::fs::write(path, b"stale socket").unwrap();
            assert!(path.exists());

            // Re-binding should detect stale file, remove it, and succeed!
            let new_listener = IpcListener::bind(&endpoint).await;
            assert!(
                new_listener.is_ok(),
                "should cleanly recover from stale socket"
            );
        }
    }
}

#[tokio::test]
async fn an_oversized_message_is_refused_instead_of_buffered() {
    use rustdm::ipc::transport::{IpcTransportError, MAX_MESSAGE_BYTES};
    let endpoint = next_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).await.expect("bind listener");
    let server_task = tokio::spawn(async move {
        let mut conn = listener.accept().await.expect("accept client");
        conn.recv().await
    });

    let mut client = IpcConnection::connect(&endpoint).await.expect("connect");
    let huge = "x".repeat(MAX_MESSAGE_BYTES as usize + 1);
    let req = RpcRequest::new(
        RpcId::Number(1),
        "download.add",
        Some(serde_json::json!({ "url": huge })),
    );
    // The server may close the connection before the whole message is written.
    let _ = client.send(&req).await;

    match server_task.await.unwrap() {
        Err(IpcTransportError::MessageTooLarge(limit)) => assert_eq!(limit, MAX_MESSAGE_BYTES),
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn only_the_owner_may_connect_to_the_socket() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner.sock");
    let _listener = IpcListener::bind(&IpcEndpoint::UnixSocket(path.clone()))
        .await
        .expect("bind listener");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
