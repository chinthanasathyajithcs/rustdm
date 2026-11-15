use rustdm::error::Error;
use rustdm::probe::{parse_content_range_total, probe};
use rustdm::testing::{pattern_bytes, Mode, TestServer};

#[tokio::test]
async fn probe_reports_size_and_range_support_on_206() {
    let body = pattern_bytes(1000);
    let srv = TestServer::start(body, Mode::Normal).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert_eq!(res.total, Some(1000));
    assert!(res.ranges_supported);
}

#[tokio::test]
async fn probe_falls_back_when_server_ignores_range() {
    let body = pattern_bytes(1000);
    let srv = TestServer::start(body, Mode::IgnoreRange).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert_eq!(res.total, Some(1000));
    assert!(!res.ranges_supported);
}

#[tokio::test]
async fn probe_records_strong_etag_and_last_modified() {
    let body = pattern_bytes(500);
    let srv = TestServer::start(body, Mode::Normal).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert_eq!(res.validator.etag.as_deref(), Some("\"v1\""));
    assert!(res.validator.last_modified.is_some());
}

#[tokio::test]
async fn probe_follows_redirect_and_records_final_url() {
    let body = pattern_bytes(500);
    let srv = TestServer::start(body, Mode::Redirect).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert!(res.final_url.path().starts_with("/moved"));
}

#[tokio::test]
async fn probe_sends_bytes_0_0_and_identity_encoding() {
    let body = pattern_bytes(500);
    let srv = TestServer::start(body, Mode::Normal).await;
    let client = reqwest::Client::new();
    let _ = probe(&client, &srv.url()).await.unwrap();
    let logs = srv.requests();
    let first = logs.first().expect("request logged");
    assert_eq!(first.range.as_deref(), Some("bytes=0-0"));
    assert_eq!(first.accept_encoding.as_deref(), Some("identity"));
}

#[tokio::test]
async fn probe_reports_link_expired_on_403() {
    let srv = TestServer::start(vec![], Mode::Status(403)).await;
    let client = reqwest::Client::new();
    let err = probe(&client, &srv.url()).await.unwrap_err();
    match err {
        Error::LinkExpired { status } => assert_eq!(status, 403),
        other => panic!("expected LinkExpired, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_reports_link_expired_on_401_without_retrying() {
    let srv = TestServer::start(vec![], Mode::Status(401)).await;
    let client = reqwest::Client::new();
    let err = probe(&client, &srv.url()).await.unwrap_err();
    match err {
        Error::LinkExpired { status } => assert_eq!(status, 401),
        other => panic!("expected LinkExpired, got {other:?}"),
    }
    assert_eq!(srv.requests().len(), 1);
}

#[tokio::test]
async fn probe_records_the_content_type() {
    let srv = TestServer::start(pattern_bytes(500), Mode::ContentType("text/html".into())).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert_eq!(res.content_type.as_deref(), Some("text/html"));
}

#[tokio::test]
async fn probe_has_no_content_type_when_the_server_sends_none() {
    let srv = TestServer::start(pattern_bytes(500), Mode::Normal).await;
    let client = reqwest::Client::new();
    let res = probe(&client, &srv.url()).await.unwrap();
    assert_eq!(res.content_type, None);
}

#[test]
fn content_range_total_parses_only_numeric_totals() {
    assert_eq!(parse_content_range_total("bytes 0-0/1000"), Some(1000));
    assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
}
