use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub fn pattern_bytes(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| {
            // Deterministic LCG pattern without short period
            let v = (i
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407)
                >> 32) as u8;
            v ^ (i as u8)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    IgnoreRange,
    Status(u16),
    Redirect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLog {
    pub method: String,
    pub path: String,
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub accept_encoding: Option<String>,
    pub user_agent: Option<String>,
}

pub struct TestServer {
    url: url::Url,
    etag: Arc<RwLock<String>>,
    requests: Arc<Mutex<Vec<RequestLog>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServer {
    pub async fn start(body: Vec<u8>, mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr: SocketAddr = listener.local_addr().unwrap();
        let url = url::Url::parse(&format!("http://{local_addr}/")).unwrap();

        let etag = Arc::new(RwLock::new("\"v1\"".to_string()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server_etag = Arc::clone(&etag);
        let server_requests = Arc::clone(&requests);
        let server_body = Arc::new(body);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    accept_res = listener.accept() => {
                        let Ok((mut socket, _)) = accept_res else {
                            break;
                        };

                        let etag_lock = Arc::clone(&server_etag);
                        let reqs_lock = Arc::clone(&server_requests);
                        let body_ref = Arc::clone(&server_body);
                        let mode_clone = mode.clone();

                        tokio::spawn(async move {
                            let mut buf = Vec::new();
                            let mut temp_buf = [0u8; 1024];

                            // Read until header boundary
                            loop {
                                match socket.read(&mut temp_buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        buf.extend_from_slice(&temp_buf[..n]);
                                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                            break;
                                        }
                                    }
                                    Err(_) => return,
                                }
                            }

                            let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                                return;
                            };

                            let header_str = match std::str::from_utf8(&buf[..pos]) {
                                Ok(s) => s,
                                Err(_) => return,
                            };

                            let mut lines = header_str.split("\r\n");
                            let request_line = lines.next().unwrap_or("");
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or("GET").to_string();
                            let path = parts.next().unwrap_or("/").to_string();

                            let mut range_hdr = None;
                            let mut if_range_hdr = None;
                            let mut accept_encoding_hdr = None;
                            let mut user_agent_hdr = None;

                            for line in lines {
                                if let Some((k, v)) = line.split_once(':') {
                                    let key = k.trim().to_ascii_lowercase();
                                    let val = v.trim().to_string();
                                    match key.as_str() {
                                        "range" => range_hdr = Some(val),
                                        "if-range" => if_range_hdr = Some(val),
                                        "accept-encoding" => accept_encoding_hdr = Some(val),
                                        "user-agent" => user_agent_hdr = Some(val),
                                        _ => {}
                                    }
                                }
                            }

                            let log_entry = RequestLog {
                                method,
                                path: path.clone(),
                                range: range_hdr.clone(),
                                if_range: if_range_hdr.clone(),
                                accept_encoding: accept_encoding_hdr,
                                user_agent: user_agent_hdr,
                            };

                            reqs_lock.lock().unwrap().push(log_entry);

                            let current_etag = etag_lock.read().unwrap().clone();
                            let last_modified = "Sun, 06 Nov 1994 08:49:37 GMT";

                            let response = match &mode_clone {
                                Mode::Redirect => {
                                    if !path.starts_with("/moved/") {
                                        let redirect_target = format!("/moved{}", if path.starts_with('/') { &path } else { "/" });
                                        format!(
                                            "HTTP/1.1 302 Found\r\nLocation: {redirect_target}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                                        )
                                        .into_bytes()
                                    } else {
                                        let total = body_ref.len();
                                        let mut resp = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                        )
                                        .into_bytes();
                                        resp.extend_from_slice(&body_ref);
                                        resp
                                    }
                                }
                                Mode::Status(status) => {
                                    format!(
                                        "HTTP/1.1 {status} Status\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                                    )
                                    .into_bytes()
                                }
                                Mode::IgnoreRange => {
                                    let total = body_ref.len();
                                    let mut resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    )
                                    .into_bytes();
                                    resp.extend_from_slice(&body_ref);
                                    resp
                                }
                                Mode::Normal => {
                                    let total = body_ref.len();
                                    let mut serve_range = None;

                                    if let Some(r) = range_hdr {
                                        if let Some(ir) = if_range_hdr {
                                            if ir == current_etag {
                                                serve_range = parse_range(&r, total);
                                            }
                                        } else {
                                            serve_range = parse_range(&r, total);
                                        }
                                    }

                                    if let Some((start, end)) = serve_range {
                                        let slice_len = end - start + 1;
                                        let mut resp = format!(
                                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                        )
                                        .into_bytes();
                                        resp.extend_from_slice(&body_ref[start..=end]);
                                        resp
                                    } else {
                                        let mut resp = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                        )
                                        .into_bytes();
                                        resp.extend_from_slice(&body_ref);
                                        resp
                                    }
                                }
                            };

                            let _ = socket.write_all(&response).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                }
            }
        });

        TestServer {
            url,
            etag,
            requests,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn url(&self) -> url::Url {
        self.url.clone()
    }

    pub fn set_etag(&self, etag: &str) {
        *self.etag.write().unwrap() = etag.to_string();
    }

    pub fn requests(&self) -> Vec<RequestLog> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn parse_range(range_hdr: &str, total: usize) -> Option<(usize, usize)> {
    let s = range_hdr.trim().strip_prefix("bytes=")?;
    let (start_str, end_str) = s.split_once('-')?;
    if start_str.is_empty() {
        let suffix: usize = end_str.parse().ok()?;
        if suffix == 0 || total == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        Some((start, total.saturating_sub(1)))
    } else {
        let start: usize = start_str.parse().ok()?;
        let end = if end_str.is_empty() {
            total.saturating_sub(1)
        } else {
            let e: usize = end_str.parse().ok()?;
            e.min(total.saturating_sub(1))
        };
        if start > end || start >= total {
            None
        } else {
            Some((start, end))
        }
    }
}
