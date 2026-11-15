use crate::cancel::{or_cancel, sleep_or_cancel};
use crate::engine::Progress;
use crate::error::{is_link_status, Error};
use crate::probe::Validator;
use crate::ranges::ByteRange;
use crate::retry::{backoff_delay, retry_after_secs, RetryCause, MAX_SEGMENT_ATTEMPTS};
use crate::scheduler::{Crew, Lane};
use crate::writer::PartFile;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, IF_RANGE, RANGE, RETRY_AFTER};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn parse_content_range_bounds(value: &str) -> Option<(u64, u64)> {
    let s = value.trim();
    let s = s.strip_prefix("bytes ")?;
    let (range_part, _total) = s.split_once('/')?;
    let (start_str, end_str) = range_part.split_once('-')?;
    let start: u64 = start_str.trim().parse().ok()?;
    let end: u64 = end_str.trim().parse().ok()?;
    Some((start, end))
}

/// How a connection's work on a lane ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneEnd {
    /// Every byte of the lane is written.
    Finished,
    /// The server refused this connection (429/503) while others of the crew were
    /// still downloading: it is at its connection limit. The lane's rest is unwritten.
    Refused,
}

pub async fn fetch_range(
    client: &reqwest::Client,
    url: &url::Url,
    validator: &Validator,
    range: ByteRange,
    part: Arc<PartFile>,
    progress: Arc<Progress>,
) -> Result<(), Error> {
    let lane = Arc::new(Lane::new(range.start, range.end, std::time::Instant::now()));
    fetch_lane(client, url, validator, lane, part, progress, None, None)
        .await
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_lane(
    client: &reqwest::Client,
    url: &url::Url,
    validator: &Validator,
    lane: Arc<Lane>,
    part: Arc<PartFile>,
    progress: Arc<Progress>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
    crew: Option<&Crew>,
) -> Result<LaneEnd, Error> {
    let mut current_offset = lane.cursor.load(Ordering::SeqCst);
    // Failed attempts in a row. Any attempt that writes bytes resets it, so a
    // connection that keeps dropping but keeps making progress never gives up.
    let mut attempt = 0u32;
    let cancel = cancel.as_ref();

    while current_offset < lane.end.load(Ordering::SeqCst) {
        if cancel.is_some_and(|rx| *rx.borrow()) {
            return Err(Error::Cancelled);
        }

        let current_end = lane.end.load(Ordering::SeqCst);
        if current_offset >= current_end {
            break;
        }

        if attempt >= MAX_SEGMENT_ATTEMPTS {
            return Err(Error::Protocol(format!(
                "max attempts ({MAX_SEGMENT_ATTEMPTS}) reached for range starting at {current_offset}"
            )));
        }

        let mut req_builder = client
            .get(url.clone())
            .header(
                RANGE,
                format!("bytes={}-{}", current_offset, current_end - 1),
            )
            .header(ACCEPT_ENCODING, "identity");

        let has_if_range = if let Some(etag) = &validator.etag {
            req_builder = req_builder.header(IF_RANGE, etag.as_str());
            true
        } else if let Some(lm) = &validator.last_modified {
            req_builder = req_builder.header(IF_RANGE, lm.as_str());
            true
        } else {
            false
        };

        let res = match or_cancel(req_builder.send(), cancel).await? {
            Ok(r) => r,
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_SEGMENT_ATTEMPTS {
                    return Err(Error::Protocol(format!(
                        "connection failed after {MAX_SEGMENT_ATTEMPTS} attempts"
                    )));
                }
                let wait = backoff_delay(attempt, fastrand::f64());
                progress.note_retry(RetryCause::of_failed_request(&e), wait);
                sleep_or_cancel(wait, cancel).await?;
                continue;
            }
        };

        let status = res.status();
        let status_code = status.as_u16();

        // 401, 403, 404, 410 -> LinkExpired at once, no retry
        if is_link_status(status_code) {
            return Err(Error::LinkExpired {
                status: status_code,
            });
        }

        // 200 on request that carried If-Range -> FileChanged at once, no retry
        if status_code == 200 && has_if_range {
            return Err(Error::FileChanged);
        }

        // 429 or 503 -> retry with Retry-After or backoff
        if status_code == 429 || status_code == 503 {
            // Others are getting through: this one is over the server's limit.
            if crew.is_some_and(Crew::others_active) {
                return Ok(LaneEnd::Refused);
            }
            attempt += 1;
            if attempt >= MAX_SEGMENT_ATTEMPTS {
                return Err(Error::Protocol(format!(
                    "server returned status {status_code} after {MAX_SEGMENT_ATTEMPTS} attempts"
                )));
            }
            let delay = res
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_secs)
                .unwrap_or_else(|| backoff_delay(attempt, fastrand::f64()));
            progress.note_retry(
                RetryCause::Busy {
                    status: status_code,
                },
                delay,
            );
            sleep_or_cancel(delay, cancel).await?;
            continue;
        }

        if status_code != 206 {
            attempt += 1;
            if attempt >= MAX_SEGMENT_ATTEMPTS {
                return Err(Error::Protocol(format!(
                    "expected 206 Partial Content, received status {status_code}"
                )));
            }
            let wait = backoff_delay(attempt, fastrand::f64());
            progress.note_retry(
                RetryCause::Unexpected {
                    status: status_code,
                },
                wait,
            );
            sleep_or_cancel(wait, cancel).await?;
            continue;
        }

        // Validate Content-Range
        let content_range = res
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());

        let valid_cr = content_range
            .and_then(parse_content_range_bounds)
            .is_some_and(|(start, _)| start == current_offset);

        if !valid_cr {
            attempt += 1;
            if attempt >= MAX_SEGMENT_ATTEMPTS {
                return Err(Error::Protocol("wrong Content-Range".to_string()));
            }
            let wait = backoff_delay(attempt, fastrand::f64());
            progress.note_retry(RetryCause::WrongPart, wait);
            sleep_or_cancel(wait, cancel).await?;
            continue;
        }

        // Stream body. A silent connection ends with a read timeout from the client.
        let mut stream = res;
        let mut stream_failed = false;
        let offset_at_start = current_offset;

        loop {
            match or_cancel(stream.chunk(), cancel).await? {
                Ok(Some(chunk)) => {
                    let end_limit = lane.end.load(Ordering::SeqCst);
                    if current_offset >= end_limit {
                        break;
                    }

                    let allowed = (end_limit - current_offset) as usize;
                    let to_write_len = chunk.len().min(allowed);
                    let to_write = chunk.slice(..to_write_len);
                    progress.throttle(to_write_len as u64, cancel).await?;

                    let part_clone = Arc::clone(&part);
                    let curr_offset = current_offset;

                    tokio::task::spawn_blocking(move || {
                        part_clone.write_at(curr_offset, &to_write)
                    })
                    .await
                    .map_err(|e| Error::Protocol(e.to_string()))
                    .and_then(|res| res.map_err(Error::from))?;

                    current_offset += to_write_len as u64;
                    lane.mark_first_byte();
                    lane.cursor.store(current_offset, Ordering::SeqCst);
                    lane.bytes_done
                        .fetch_add(to_write_len as u64, Ordering::SeqCst);
                    progress
                        .downloaded
                        .fetch_add(to_write_len as u64, Ordering::SeqCst);
                    progress.completed.lock().unwrap().insert(ByteRange {
                        start: curr_offset,
                        end: current_offset,
                    });

                    if current_offset >= end_limit {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    stream_failed = true;
                    break;
                }
            }
        }

        let target_end = lane.end.load(Ordering::SeqCst);
        if stream_failed && current_offset < target_end {
            // It was making progress: reconnect at once, with a fresh set of attempts.
            if current_offset > offset_at_start {
                attempt = 0;
                progress.note_retry(RetryCause::Dropped, std::time::Duration::ZERO);
                continue;
            }
            attempt += 1;
            if attempt >= MAX_SEGMENT_ATTEMPTS {
                return Err(Error::Protocol(format!(
                    "short body or connection drop reached max attempts ({MAX_SEGMENT_ATTEMPTS})"
                )));
            }
            let wait = backoff_delay(attempt, fastrand::f64());
            progress.note_retry(RetryCause::Dropped, wait);
            sleep_or_cancel(wait, cancel).await?;
            continue;
        }
    }

    Ok(LaneEnd::Finished)
}
