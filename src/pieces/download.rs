//! Fetching a stream's pieces several at a time, through the download's speed
//! caps and stop signal, and writing them in order (spec 1c, sections 7.3, 7.4).

use super::{decrypt, may_start, Piece, PieceStore, UNREADABLE_PLAYLIST};
use crate::cancel::{or_cancel, sleep_or_cancel};
use crate::engine::Progress;
use crate::error::{is_link_status, Error};
use crate::retry::{backoff_delay, retry_after_secs, RetryCause, MAX_SEGMENT_ATTEMPTS};
use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

/// An AES-128 key's bytes and the IV of the piece it opens.
type Sealed = Option<([u8; 16], [u8; 16])>;

/// How many pieces are finished, of how many; the row's `piece 140 of 312`.
#[derive(Debug, Default)]
pub struct PieceCounter {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

/// One stream to fetch as pieces.
pub struct PiecesRequest {
    pub client: reqwest::Client,
    pub pieces: Vec<Piece>,
    /// The stream file to make.
    pub file: PathBuf,
    pub connections: u32,
    pub progress: Arc<Progress>,
    pub counter: Arc<PieceCounter>,
    pub cancel: watch::Receiver<bool>,
}

/// Fetch every piece into `req.file`. Stops with `Error::Cancelled` when told
/// to, and with `Error::LinkExpired` when a piece's link is refused; either way
/// the pieces written so far are kept for the next run, and the progress then
/// counts only them: a piece cut short is fetched whole again.
pub async fn download(req: PiecesRequest) -> Result<PathBuf, Error> {
    *req.progress.output.lock().unwrap() = Some(req.file.clone());
    let mut store = PieceStore::open(&req.file, &req.pieces)?;
    // Pieces kept before a crash, but not yet written, go in first: the
    // scheduler starts only pieces that are neither written nor waiting.
    store.flush()?;
    req.progress
        .downloaded
        .store(store.kept_bytes(), Ordering::Relaxed);
    req.counter
        .total
        .store(req.pieces.len() as u64, Ordering::Relaxed);
    req.counter.done.store(
        (store.written() + store.waiting().len()) as u64,
        Ordering::Relaxed,
    );
    let mut running = JoinSet::new();
    if let Err(e) = fetch_all(&req, &mut store, &mut running).await {
        // The stopped pieces must not add bytes after the count below.
        running.shutdown().await;
        req.progress
            .downloaded
            .store(store.kept_bytes(), Ordering::Relaxed);
        return Err(e);
    }
    store.finish()
}

type Running = JoinSet<(usize, Result<Vec<u8>, Error>)>;

/// Run up to `connections` pieces at once until every piece is written.
async fn fetch_all(
    req: &PiecesRequest,
    store: &mut PieceStore,
    running: &mut Running,
) -> Result<(), Error> {
    let PiecesRequest {
        client,
        pieces,
        connections,
        progress,
        counter,
        cancel,
        ..
    } = req;
    let connections = (*connections).max(1);
    let mut in_flight = BTreeSet::new();
    let mut keys: HashMap<Url, [u8; 16]> = HashMap::new();
    while store.written() < pieces.len() {
        if *cancel.borrow() {
            return Err(Error::Cancelled);
        }
        let waiting = store.waiting();
        while running.len() < connections as usize {
            let first = store.written();
            let next = (first..pieces.len())
                .take_while(|&i| may_start(i, first, connections))
                .find(|i| !waiting.contains(i) && !in_flight.contains(i));
            let Some(index) = next else {
                break;
            };
            let sealed = match &pieces[index].key {
                Some(key) => Some((
                    key_bytes(&mut keys, client, &key.url, cancel).await?,
                    key.iv,
                )),
                None => None,
            };
            in_flight.insert(index);
            let fetch = fetch_piece(
                client.clone(),
                pieces[index].clone(),
                sealed,
                Arc::clone(progress),
                cancel.clone(),
            );
            running.spawn(async move { (index, fetch.await) });
        }
        let Some(joined) = running.join_next().await else {
            return Err(Error::State("no piece is running".to_string()));
        };
        let (index, fetched) = joined.map_err(|e| Error::State(e.to_string()))?;
        in_flight.remove(&index);
        store.keep(index, &fetched?)?;
        store.flush()?;
        counter.done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// Fetch one piece, retrying what may pass (a dropped connection, a busy or
/// failing server) up to the usual number of attempts.
async fn fetch_piece(
    client: reqwest::Client,
    piece: Piece,
    sealed: Sealed,
    progress: Arc<Progress>,
    cancel: watch::Receiver<bool>,
) -> Result<Vec<u8>, Error> {
    let cancel = Some(&cancel);
    let mut attempt = 0u32;
    loop {
        let retry = match attempt_piece(&client, &piece, &progress, cancel).await? {
            Attempt::Done(bytes) => {
                return match sealed {
                    Some((key, iv)) => decrypt(&key, &iv, &bytes),
                    None => Ok(bytes),
                }
            }
            Attempt::Retry(cause, wait) => (cause, wait),
        };
        attempt += 1;
        if attempt >= MAX_SEGMENT_ATTEMPTS {
            return Err(Error::Protocol(format!(
                "a piece failed after {MAX_SEGMENT_ATTEMPTS} attempts"
            )));
        }
        let (cause, wait) = retry;
        let wait = wait.unwrap_or_else(|| backoff_delay(attempt, fastrand::f64()));
        progress.note_retry(cause, wait);
        sleep_or_cancel(wait, cancel).await?;
    }
}

/// The key at `url`, fetched once per run and kept in memory only.
async fn key_bytes(
    keys: &mut HashMap<Url, [u8; 16]>,
    client: &reqwest::Client,
    url: &Url,
    cancel: &watch::Receiver<bool>,
) -> Result<[u8; 16], Error> {
    if let Some(key) = keys.get(url) {
        return Ok(*key);
    }
    let response = or_cancel(client.get(url.clone()).send(), Some(cancel)).await??;
    let status = response.status().as_u16();
    if is_link_status(status) {
        return Err(Error::LinkExpired { status });
    }
    if !response.status().is_success() {
        return Err(Error::Protocol(format!(
            "the server answered the video's key with status {status}"
        )));
    }
    let bytes = or_cancel(response.bytes(), Some(cancel)).await??;
    let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| Error::Video {
        message: UNREADABLE_PLAYLIST.to_string(),
        transient: false,
    })?;
    keys.insert(url.clone(), key);
    Ok(key)
}

/// How one request for a piece ended.
enum Attempt {
    Done(Vec<u8>),
    /// Try again, after this long when the server said.
    Retry(RetryCause, Option<Duration>),
}

async fn attempt_piece(
    client: &reqwest::Client,
    piece: &Piece,
    progress: &Progress,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<Attempt, Error> {
    let mut request = client.get(piece.url.clone());
    if let Some(slice) = piece.range {
        let last = slice.offset + slice.len.saturating_sub(1);
        request = request.header(RANGE, format!("bytes={}-{last}", slice.offset));
    }
    let mut response = match or_cancel(request.send(), cancel).await? {
        Ok(response) => response,
        Err(e) => return Ok(Attempt::Retry(RetryCause::of_failed_request(&e), None)),
    };
    let status = response.status().as_u16();
    if is_link_status(status) {
        return Err(Error::LinkExpired { status });
    }
    if status == 429 || status == 503 {
        let after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after_secs);
        return Ok(Attempt::Retry(RetryCause::Busy { status }, after));
    }
    if status >= 500 {
        return Ok(Attempt::Retry(RetryCause::Unexpected { status }, None));
    }
    if let Some(slice) = piece.range {
        if status == 200 {
            return Err(Error::Protocol(
                "the server sent a whole file for one slice of it".to_string(),
            ));
        }
        let start = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(range_start);
        if status != 206 || start != Some(slice.offset) {
            return Ok(Attempt::Retry(RetryCause::WrongPart, None));
        }
    } else if !(200..300).contains(&status) {
        return Err(Error::Protocol(format!(
            "the server answered a piece with status {status}"
        )));
    }
    let mut bytes = Vec::new();
    loop {
        match or_cancel(response.chunk(), cancel).await? {
            Ok(Some(chunk)) => {
                progress.throttle(chunk.len() as u64, cancel).await?;
                progress
                    .downloaded
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Ok(lost(progress, &bytes, RetryCause::Dropped)),
        }
    }
    if piece.range.is_some_and(|s| bytes.len() as u64 != s.len) {
        return Ok(lost(progress, &bytes, RetryCause::WrongPart));
    }
    Ok(Attempt::Done(bytes))
}

/// The bytes of a failed attempt no longer count as downloaded.
fn lost(progress: &Progress, bytes: &[u8], cause: RetryCause) -> Attempt {
    progress
        .downloaded
        .fetch_sub(bytes.len() as u64, Ordering::Relaxed);
    Attempt::Retry(cause, None)
}

/// The first byte of `bytes start-end/total`.
fn range_start(value: &str) -> Option<u64> {
    let range = value.trim().strip_prefix("bytes ")?;
    range.split('-').next()?.trim().parse().ok()
}
