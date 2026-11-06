use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    Chunked,
    FailThenOk {
        status: u16,
        times: u32,
        retry_after: Option<u32>,
    },
    ShortBody,
    WrongContentRange,
    DropMidBodyOnce,
    SlowLowHalf {
        bytes_per_sec: u64,
    },
    ExpireAfter(u32),
    Throttle {
        bytes_per_sec: u64,
    },
    SimulateSize(u64),
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
    peak_in_flight: Arc<AtomicUsize>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

/// The `Range` header of the engine's size probe.
const PROBE_RANGE: &str = "bytes=0-0";

/// Counts a request as in flight until dropped, and records the highest count seen.
struct InFlight {
    current: Arc<AtomicUsize>,
}

impl InFlight {
    fn enter(current: &Arc<AtomicUsize>, peak: &AtomicUsize) -> Self {
        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        Self {
            current: Arc::clone(current),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TestServer {
    pub async fn start(body: Vec<u8>, mode: Mode) -> Self {
        Self::start_at("127.0.0.1:0", body, mode).await.unwrap()
    }

    pub async fn start_at(bind_addr: &str, body: Vec<u8>, mode: Mode) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr: SocketAddr = listener.local_addr()?;
        let url = url::Url::parse(&format!("http://{local_addr}/")).unwrap();

        let etag = Arc::new(RwLock::new("\"v1\"".to_string()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let server_peak = Arc::clone(&peak_in_flight);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server_etag = Arc::clone(&etag);
        let server_requests = Arc::clone(&requests);
        let server_body = Arc::new(body);
        let mode_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));

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
                        let counter = Arc::clone(&mode_counter);
                        let in_flight = Arc::clone(&in_flight);
                        let peak = Arc::clone(&server_peak);

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
                            // The engine's one-byte probe is not a data connection; its
                            // client stops waiting after the headers, so leave it out.
                            let is_probe = range_hdr.as_deref() == Some(PROBE_RANGE);
                            let _in_flight =
                                (!is_probe).then(|| InFlight::enter(&in_flight, &peak));

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
                                Mode::FailThenOk {
                                    status,
                                    times,
                                    retry_after,
                                } => {
                                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if count < *times {
                                        let ra_hdr = if let Some(sec) = retry_after {
                                            format!("Retry-After: {sec}\r\n")
                                        } else {
                                            String::new()
                                        };
                                        format!(
                                            "HTTP/1.1 {status} Status\r\n{ra_hdr}Connection: close\r\nContent-Length: 0\r\n\r\n"
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
                                Mode::ShortBody => {
                                    let total = body_ref.len();
                                    let serve_range = range_hdr
                                        .as_deref()
                                        .and_then(|r| parse_range(r, total))
                                        .unwrap_or((0, total.saturating_sub(1)));
                                    let (start, end) = serve_range;
                                    let slice_len = end - start + 1;
                                    let short_len = (slice_len / 2).max(1);
                                    let mut resp = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    )
                                    .into_bytes();
                                    resp.extend_from_slice(&body_ref[start..start + short_len]);
                                    resp
                                }
                                Mode::WrongContentRange => {
                                    let total = body_ref.len();
                                    let mut resp = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 9999-9999/{total}\r\nContent-Length: 1\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    )
                                    .into_bytes();
                                    resp.push(42);
                                    resp
                                }
                                Mode::DropMidBodyOnce => {
                                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    let total = body_ref.len();
                                    let serve_range = range_hdr
                                        .as_deref()
                                        .and_then(|r| parse_range(r, total))
                                        .unwrap_or((0, total.saturating_sub(1)));
                                    let (start, end) = serve_range;
                                    let slice_len = end - start + 1;
                                    if count == 0 {
                                        let part_len = (slice_len / 2).max(1);
                                        let mut resp = format!(
                                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                        )
                                        .into_bytes();
                                        resp.extend_from_slice(&body_ref[start..start + part_len]);
                                        resp
                                    } else {
                                        let mut resp = format!(
                                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                        )
                                        .into_bytes();
                                        resp.extend_from_slice(&body_ref[start..=end]);
                                        resp
                                    }
                                }
                                Mode::SlowLowHalf { bytes_per_sec } => {
                                    let total = body_ref.len();
                                    let serve_range = range_hdr
                                        .as_deref()
                                        .and_then(|r| parse_range(r, total))
                                        .unwrap_or((0, total.saturating_sub(1)));
                                    let (start, end) = serve_range;
                                    let slice_len = end - start + 1;
                                    let header = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    );
                                    let _ = socket.write_all(header.as_bytes()).await;

                                    let slice = &body_ref[start..=end];
                                    if (start as u64) < (total as u64) / 2 {
                                        let chunk_size = (*bytes_per_sec as usize / 10).max(512).min(slice.len());
                                        let delay_ms = (chunk_size as u64 * 1000) / bytes_per_sec.max(&1);
                                        for chunk in slice.chunks(chunk_size) {
                                            if socket.write_all(chunk).await.is_err() {
                                                break;
                                            }
                                            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                                        }
                                    } else {
                                        let _ = socket.write_all(slice).await;
                                    }
                                    let _ = socket.shutdown().await;
                                    return;
                                }
                                Mode::Chunked => {
                                    let mut resp =
                                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                                            .to_vec();
                                    for chunk in body_ref.chunks(1024) {
                                        resp.extend_from_slice(
                                            format!("{:X}\r\n", chunk.len()).as_bytes(),
                                        );
                                        resp.extend_from_slice(chunk);
                                        resp.extend_from_slice(b"\r\n");
                                    }
                                    resp.extend_from_slice(b"0\r\n\r\n");
                                    resp
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
                                Mode::ExpireAfter(limit) => {
                                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if count >= *limit {
                                        b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_vec()
                                    } else {
                                        let total = body_ref.len();
                                        let mut serve_range = None;
                                        if let Some(r) = range_hdr {
                                            serve_range = parse_range(&r, total);
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
                                }
                                Mode::Throttle { bytes_per_sec } => {
                                    let total = body_ref.len();
                                    let serve_range = range_hdr
                                        .as_deref()
                                        .and_then(|r| parse_range(r, total))
                                        .unwrap_or((0, total.saturating_sub(1)));
                                    let (start, end) = serve_range;
                                    let slice_len = end - start + 1;
                                    let header = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    );
                                    let _ = socket.write_all(header.as_bytes()).await;

                                    let slice = &body_ref[start..=end];
                                    let chunk_size = (*bytes_per_sec as usize / 10).max(512).min(slice.len());
                                    let delay_ms = (chunk_size as u64 * 1000) / bytes_per_sec.max(&1);
                                    // Wait before each chunk, not after, so a finished
                                    // response stops counting as in flight right away.
                                    for chunk in slice.chunks(chunk_size) {
                                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                                        if socket.write_all(chunk).await.is_err() {
                                            break;
                                        }
                                    }
                                    let _ = socket.shutdown().await;
                                    return;
                                }
                                Mode::SimulateSize(fake_total) => {
                                    let total = *fake_total;
                                    let mut serve_range = (0, 0);
                                    if let Some(r) = range_hdr {
                                        if let Some((s, e)) = parse_range(&r, body_ref.len()) {
                                            serve_range = (s, e);
                                        }
                                    }
                                    let (start, end) = serve_range;
                                    let slice_len = end - start + 1;
                                    let mut resp = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {slice_len}\r\nETag: {current_etag}\r\nLast-Modified: {last_modified}\r\nConnection: close\r\n\r\n"
                                    )
                                    .into_bytes();
                                    let body_slice = if !body_ref.is_empty() {
                                        let end_idx = (start + slice_len).min(body_ref.len());
                                        &body_ref[start..end_idx]
                                    } else {
                                        &[]
                                    };
                                    resp.extend_from_slice(body_slice);
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

        Ok(TestServer {
            url,
            etag,
            requests,
            peak_in_flight,
            shutdown_tx: Some(shutdown_tx),
        })
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

    /// The most data requests this server has been answering at the same time.
    /// The engine's `bytes=0-0` size probe is not counted.
    pub fn peak_concurrent_requests(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
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
