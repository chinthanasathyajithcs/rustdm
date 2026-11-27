//! A download's cookies, Referer and User-Agent reach the server on every request.

use rustdm::engine::{build_client, download, DownloadRequest, Outcome, Progress, Timeouts};
use rustdm::request_context::{client_for, ContextCookie, RequestContext};
use rustdm::testing::{pattern_bytes, Mode, RequestLog, TestServer};
use std::sync::Arc;
use tempfile::tempdir;

const REFERER: &str = "https://files.example.com/page";
const USER_AGENT: &str = "TestBrowser/1.0 (rdm test)";

fn sign_in_cookie() -> ContextCookie {
    ContextCookie {
        name: "sid".to_string(),
        value: "abc123".to_string(),
        domain: "127.0.0.1".to_string(),
        path: "/".to_string(),
        secure: false,
        host_only: true,
        expires: None,
    }
}

fn full_context() -> RequestContext {
    RequestContext {
        cookies: vec![sign_in_cookie()],
        referer: Some(REFERER.to_string()),
        user_agent: Some(USER_AGENT.to_string()),
        title: None,
    }
}

async fn fetch(client: &reqwest::Client, url: url::Url, connections: u32) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let req = DownloadRequest {
        url,
        template: "{downloads}/{name}".to_string(),
        file_name: Some("out.bin".to_string()),
        downloads_dir: dir.path().to_path_buf(),
        connections,
        progress: Arc::new(Progress::new()),
        cancel: None,
        max_request_bytes: None,
    };
    match download(client, req).await.unwrap() {
        Outcome::Completed { path, .. } => std::fs::read(path).unwrap(),
        Outcome::Paused { reason, .. } => panic!("download paused: {reason:?}"),
    }
}

fn cookie_of(request: &RequestLog) -> Option<&str> {
    request.cookie.as_deref()
}

#[tokio::test]
async fn cookie_referer_and_user_agent_reach_the_probe_and_every_worker() {
    let body = pattern_bytes(5 * 1024 * 1024);
    let server = TestServer::start(body.clone(), Mode::Normal).await;
    server.require_header("cookie", "sid=abc123");
    server.require_header("referer", REFERER);
    server.require_header("user-agent", USER_AGENT);

    let client = client_for(&full_context(), Timeouts::default()).unwrap();
    let saved = fetch(&client, server.url().join("file.bin").unwrap(), 4).await;

    assert_eq!(saved, body);
    let requests = server.requests();
    assert!(
        requests.len() >= 5,
        "probe and four workers expected, got {}",
        requests.len()
    );
    for request in &requests {
        assert_eq!(cookie_of(request), Some("sid=abc123"), "{request:?}");
        // The log keeps every header, names in lower case.
        assert!(
            request
                .headers
                .contains(&("cookie".to_string(), "sid=abc123".to_string())),
            "{request:?}"
        );
        assert_eq!(request.referer.as_deref(), Some(REFERER), "{request:?}");
        assert_eq!(
            request.user_agent.as_deref(),
            Some(USER_AGENT),
            "{request:?}"
        );
    }
}

#[tokio::test]
async fn a_download_without_context_keeps_the_default_user_agent_and_sends_no_cookie() {
    let server = TestServer::start(pattern_bytes(64 * 1024), Mode::Normal).await;
    fetch(&build_client(), server.url().join("file.bin").unwrap(), 2).await;

    for request in server.requests() {
        let agent = request.user_agent.as_deref().unwrap_or_default();
        assert!(agent.starts_with("rustdm/"), "{request:?}");
        assert_eq!(request.cookie, None);
        assert_eq!(request.referer, None);
    }
}

#[tokio::test]
async fn a_context_without_a_user_agent_keeps_the_default_one() {
    let server = TestServer::start(pattern_bytes(64 * 1024), Mode::Normal).await;
    let ctx = RequestContext {
        referer: Some(REFERER.to_string()),
        ..Default::default()
    };
    let client = client_for(&ctx, Timeouts::default()).unwrap();
    fetch(&client, server.url().join("file.bin").unwrap(), 2).await;

    for request in server.requests() {
        assert!(request.user_agent.unwrap().starts_with("rustdm/"));
        assert_eq!(request.referer.as_deref(), Some(REFERER));
    }
}

#[tokio::test]
async fn a_host_only_cookie_is_not_sent_after_a_redirect_to_another_host() {
    let body = pattern_bytes(256 * 1024);
    let target = TestServer::start(body.clone(), Mode::Normal).await;
    let port = target.url().port().unwrap();
    let by_name = url::Url::parse(&format!("http://localhost:{port}/")).unwrap();
    let redirector = TestServer::start(Vec::new(), Mode::RedirectTo(by_name)).await;

    let client = client_for(&full_context(), Timeouts::default()).unwrap();
    let saved = fetch(&client, redirector.url().join("file.bin").unwrap(), 2).await;

    assert_eq!(saved, body);
    let first_hop = redirector.requests();
    assert!(first_hop.iter().all(|r| cookie_of(r) == Some("sid=abc123")));
    let second_hop = target.requests();
    assert!(!second_hop.is_empty());
    for request in second_hop {
        assert_eq!(request.cookie, None, "{request:?}");
        // The Referer and User-Agent are not host-bound and stay.
        assert_eq!(request.referer.as_deref(), Some(REFERER));
        assert_eq!(request.user_agent.as_deref(), Some(USER_AGENT));
    }
}

#[tokio::test]
async fn a_host_only_cookie_follows_a_redirect_to_the_same_host() {
    let body = pattern_bytes(256 * 1024);
    let target = TestServer::start(body.clone(), Mode::Normal).await;
    let same_host = url::Url::parse(&format!(
        "http://127.0.0.1:{}/",
        target.url().port().unwrap()
    ))
    .unwrap();
    let redirector = TestServer::start(Vec::new(), Mode::RedirectTo(same_host)).await;

    let client = client_for(&full_context(), Timeouts::default()).unwrap();
    let saved = fetch(&client, redirector.url().join("file.bin").unwrap(), 2).await;

    assert_eq!(saved, body);
    let hops = target.requests();
    assert!(!hops.is_empty());
    assert!(hops.iter().all(|r| cookie_of(r) == Some("sid=abc123")));
}

#[tokio::test]
async fn a_cookie_set_during_a_redirect_is_used_for_the_rest_of_the_download() {
    let body = pattern_bytes(512 * 1024);
    let server = TestServer::start(body.clone(), Mode::SetCookieThenRedirect).await;

    let client = client_for(&RequestContext::default(), Timeouts::default()).unwrap();
    let saved = fetch(&client, server.url().join("file.bin").unwrap(), 3).await;

    assert_eq!(saved, body);
    let after_redirect: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/moved/"))
        .collect();
    assert!(after_redirect.len() >= 2, "{after_redirect:?}");
    for request in after_redirect {
        assert_eq!(
            cookie_of(&request),
            Some("sid=redirect-cookie"),
            "{request:?}"
        );
    }
}
