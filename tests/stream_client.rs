//! A video stream's own headers reach the server, on top of the browser's cookies.

use rustdm::engine::Timeouts;
use rustdm::request_context::{stream_client, ContextCookie, RequestContext};
use rustdm::testing::{Mode, TestServer};

const BROWSER_USER_AGENT: &str = "TestBrowser/1.0 (rdm test)";
const BROWSER_REFERER: &str = "https://browser.example.com/watch";
const STREAM_USER_AGENT: &str = "com.google.android.youtube/19.1 (stream)";
const STREAM_REFERER: &str = "https://www.youtube.com/";

fn cookie(name: &str, value: &str, domain: &str) -> ContextCookie {
    ContextCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: domain.to_string(),
        path: "/".to_string(),
        secure: false,
        host_only: true,
        expires: None,
    }
}

fn browser_context() -> RequestContext {
    RequestContext {
        cookies: vec![
            cookie("sid", "abc123", "127.0.0.1"),
            cookie("other", "nope", "other.test"),
        ],
        referer: Some(BROWSER_REFERER.to_string()),
        user_agent: Some(BROWSER_USER_AGENT.to_string()),
        title: None,
    }
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

async fn get_one(client: &reqwest::Client, server: &TestServer) {
    let response = client
        .get(server.url().join("file.bin").unwrap())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
}

#[tokio::test]
async fn a_stream_client_sends_the_stream_headers_and_the_streams_user_agent() {
    let server = TestServer::start(vec![0u8; 16], Mode::Normal).await;
    let headers = vec![
        pair("Accept", "*/*"),
        pair("Accept-Language", "en-us,en;q=0.5"),
        pair("Sec-Fetch-Mode", "navigate"),
        pair("User-Agent", STREAM_USER_AGENT),
        pair("Referer", STREAM_REFERER),
    ];

    let client = stream_client(Some(&browser_context()), &headers, Timeouts::default()).unwrap();
    get_one(&client, &server).await;

    let requests = server.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        for (name, value) in [
            ("accept", "*/*"),
            ("accept-language", "en-us,en;q=0.5"),
            ("sec-fetch-mode", "navigate"),
        ] {
            assert!(
                request.headers.contains(&pair(name, value)),
                "missing {name}: {request:?}"
            );
        }
        assert_eq!(request.user_agent.as_deref(), Some(STREAM_USER_AGENT));
        assert_eq!(request.referer.as_deref(), Some(STREAM_REFERER));
    }
}

#[tokio::test]
async fn a_stream_client_keeps_the_browser_cookies_for_their_own_host_only() {
    let server = TestServer::start(vec![0u8; 16], Mode::Normal).await;
    let headers = vec![pair("Accept", "*/*")];

    let client = stream_client(Some(&browser_context()), &headers, Timeouts::default()).unwrap();
    get_one(&client, &server).await;

    let requests = server.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request.cookie.as_deref(), Some("sid=abc123"), "{request:?}");
    }
}

#[tokio::test]
async fn a_stream_client_without_a_context_sends_the_stream_headers_and_no_cookies() {
    let server = TestServer::start(vec![0u8; 16], Mode::Normal).await;
    let headers = vec![pair("Accept", "*/*"), pair("User-Agent", STREAM_USER_AGENT)];

    let client = stream_client(None, &headers, Timeouts::default()).unwrap();
    get_one(&client, &server).await;

    let request = server.requests().remove(0);
    assert_eq!(request.user_agent.as_deref(), Some(STREAM_USER_AGENT));
    assert!(request.headers.contains(&pair("accept", "*/*")));
    assert_eq!(request.cookie, None);
}

#[tokio::test]
async fn a_stream_client_skips_a_header_it_cannot_send() {
    let server = TestServer::start(vec![0u8; 16], Mode::Normal).await;
    let headers = vec![
        pair("Bad Name", "x"),
        pair("Accept", "bad\nvalue"),
        pair("Origin", "https://www.youtube.com"),
    ];

    let client = stream_client(None, &headers, Timeouts::default()).unwrap();
    get_one(&client, &server).await;

    let request = server.requests().remove(0);
    assert!(request
        .headers
        .contains(&pair("origin", "https://www.youtube.com")));
    assert!(!request.headers.iter().any(|(name, _)| name == "bad name"));
    assert!(!request.headers.contains(&pair("accept", "bad\nvalue")));
}
