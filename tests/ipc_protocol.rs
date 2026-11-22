use rustdm::ipc::protocol::{
    encode_message, parse_rpc_message, AddDownloadParams, DownloadIdParam, DownloadStatus,
    DownloadSummary, RpcError, RpcId, RpcMessage, RpcRequest, RpcResponse, CODE_DOWNLOAD_NOT_FOUND,
};
use std::path::PathBuf;

#[test]
fn test_integration_full_rpc_cycle() {
    // 1. Client creates AddDownload request
    let add_params = AddDownloadParams {
        url: "https://speed.hetzner.de/100MB.bin".to_string(),
        destination: Some(PathBuf::from("/downloads")),
        filename: Some("100MB.bin".to_string()),
        connections: Some(16),
        max_bytes_per_sec: None,
        context: None,
        quality: None,
    };
    let client_req = RpcRequest::new(
        RpcId::Number(1001),
        "download.add",
        Some(serde_json::to_value(&add_params).unwrap()),
    );
    let wire_data = encode_message(&client_req).expect("should encode");

    // 2. Server receives wire data and parses
    let parsed_server_msg = parse_rpc_message(&wire_data).expect("server should parse message");
    let server_req = match parsed_server_msg {
        RpcMessage::Request(r) => r,
        other => panic!("expected request, got {:?}", other),
    };
    assert_eq!(server_req.method, "download.add");
    assert_eq!(server_req.id, RpcId::Number(1001));

    // 3. Server processes and sends success response
    let summary = DownloadSummary {
        id: "dl_01j7xyz".to_string(),
        url: add_params.url.clone(),
        destination: PathBuf::from("/downloads/100MB.bin"),
        filename: "100MB.bin".to_string(),
        status: DownloadStatus::Queued,
        downloaded_bytes: 0,
        total_bytes: Some(104857600),
        speed_bps: 0,
        eta_secs: None,
        error_message: None,
        max_bytes_per_sec: 0,
        video: None,
        step: None,
    };
    let server_resp = RpcResponse::success(server_req.id, serde_json::to_value(&summary).unwrap());
    let wire_resp = encode_message(&server_resp).expect("should encode response");

    // 4. Client parses response
    let client_received = parse_rpc_message(&wire_resp).expect("client should parse response");
    match client_received {
        RpcMessage::Response(resp) => {
            assert_eq!(resp.id, RpcId::Number(1001));
            assert!(resp.error.is_none());
            let res_summary: DownloadSummary =
                serde_json::from_value(resp.result.unwrap()).expect("should deserialize summary");
            assert_eq!(res_summary.id, "dl_01j7xyz");
            assert_eq!(res_summary.status, DownloadStatus::Queued);
        }
        other => panic!("expected response, got {:?}", other),
    }
}

#[test]
fn test_integration_error_handling_cycle() {
    // Client tries to pause non-existent download
    let pause_params = DownloadIdParam {
        id: "dl_non_existent".to_string(),
    };
    let client_req = RpcRequest::new(
        RpcId::String("req_pause".to_string()),
        "download.pause",
        Some(serde_json::to_value(&pause_params).unwrap()),
    );
    let wire_req = encode_message(&client_req).unwrap();

    let server_req = match parse_rpc_message(&wire_req).unwrap() {
        RpcMessage::Request(r) => r,
        _ => panic!("expected request"),
    };

    let err = RpcError::download_not_found(&pause_params.id);
    let server_resp = RpcResponse::error(server_req.id, err);
    let wire_resp = encode_message(&server_resp).unwrap();

    let client_resp = match parse_rpc_message(&wire_resp).unwrap() {
        RpcMessage::Response(r) => r,
        _ => panic!("expected response"),
    };

    assert_eq!(client_resp.id, RpcId::String("req_pause".to_string()));
    assert!(client_resp.result.is_none());
    let client_err = client_resp.error.unwrap();
    assert_eq!(client_err.code, CODE_DOWNLOAD_NOT_FOUND);
    assert!(client_err.message.contains("dl_non_existent"));
    assert_eq!(
        client_err.data.unwrap()["id"],
        serde_json::Value::String("dl_non_existent".to_string())
    );
}

#[test]
fn an_add_without_context_sends_no_context_field_and_parses_without_one() {
    let params = AddDownloadParams {
        url: "https://example.com/f.bin".to_string(),
        destination: None,
        filename: None,
        connections: None,
        max_bytes_per_sec: None,
        context: None,
        quality: None,
    };
    let json = serde_json::to_value(&params).unwrap();
    assert!(json.get("context").is_none());

    let older: AddDownloadParams =
        serde_json::from_str(r#"{"url":"https://example.com/f.bin"}"#).unwrap();
    assert_eq!(older.context, None);
}

#[test]
fn an_add_carries_the_browser_context_from_the_documented_json() {
    let json = r#"{
        "url": "https://example.com/f.bin",
        "context": {
            "cookies": [{"name": "SID", "value": "x", "domain": ".example.com", "path": "/",
                         "secure": true, "host_only": false, "expires": 1767225600.5}],
            "referer": "https://example.com/files",
            "user_agent": "Mozilla/5.0"
        }
    }"#;
    let params: AddDownloadParams = serde_json::from_str(json).unwrap();
    let context = params.context.expect("context");
    assert_eq!(context.cookies.len(), 1);
    assert_eq!(context.cookies[0].expires, Some(1_767_225_600));
    assert_eq!(
        context.referer.as_deref(),
        Some("https://example.com/files")
    );
    assert_eq!(context.user_agent.as_deref(), Some("Mozilla/5.0"));

    let wire = serde_json::to_string(&context).unwrap();
    assert!(
        wire.contains("\"value\":\"x\""),
        "the wire format keeps the value"
    );
    assert!(!format!("{context:?}").contains("\"x\""));
}
