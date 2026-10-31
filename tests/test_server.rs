use rustdm::testing::{pattern_bytes, Mode, TestServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[allow(dead_code)]
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn raw_get(srv: &TestServer, range: &str, if_range: Option<&str>) -> RawResponse {
    let addr = srv.url().socket_addrs(|| None).unwrap()[0];
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let mut req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nRange: {}\r\nConnection: close\r\n",
        srv.url().host_str().unwrap(),
        range
    );
    if let Some(ir) = if_range {
        req.push_str(&format!("If-Range: {}\r\n", ir));
    }
    req.push_str("\r\n");

    stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();

    // Parse header and body split by \r\n\r\n
    let pos = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("valid http response header");
    let header_str = std::str::from_utf8(&buf[..pos]).unwrap();
    let body = buf[pos + 4..].to_vec();

    let mut lines = header_str.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    RawResponse {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn ranged_get_returns_slice_and_stale_if_range_returns_full() {
    let body = pattern_bytes(1000);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let slice = raw_get(&srv, "bytes=10-19", None).await;
    assert_eq!(slice.status, 206);
    assert_eq!(slice.body, body[10..20]);
    let full = raw_get(&srv, "bytes=10-19", Some("\"stale\"")).await;
    assert_eq!(full.status, 200);
    assert_eq!(full.body.len(), 1000);
}
