use crate::cancel::{or_cancel, sleep_or_cancel};
use crate::engine::Progress;
use crate::error::Error;
use crate::retry::{backoff_delay, retry_after_secs, RetryCause, MAX_SEGMENT_ATTEMPTS};
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED,
    RANGE, RETRY_AFTER,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validator {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub final_url: url::Url,
    pub total: Option<u64>,
    pub ranges_supported: bool,
    pub validator: Validator,
    pub content_disposition: Option<String>,
}

pub fn parse_content_range_total(value: &str) -> Option<u64> {
    let s = value.trim();
    let (_range_part, total_str) = s.rsplit_once('/')?;
    let total_str = total_str.trim();
    if total_str == "*" {
        None
    } else {
        total_str.parse::<u64>().ok()
    }
}

pub async fn probe(client: &reqwest::Client, url: &url::Url) -> Result<Probe, Error> {
    probe_with_cancel(client, url, None).await
}

/// `probe`, stopping with `Error::Cancelled` as soon as `cancel` turns true. A
/// server that has gone silent would otherwise hold a pause or cancel for
/// every attempt's connection timeout and the waits between them.
pub async fn probe_with_cancel(
    client: &reqwest::Client,
    url: &url::Url,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<Probe, Error> {
    probe_for(client, url, cancel, None).await
}

/// `probe_with_cancel`, noting on `progress` why each failed attempt is retried.
pub async fn probe_for(
    client: &reqwest::Client,
    url: &url::Url,
    cancel: Option<&watch::Receiver<bool>>,
    progress: Option<&Progress>,
) -> Result<Probe, Error> {
    let note = |cause, wait| {
        if let Some(p) = progress {
            p.note_retry(cause, wait);
        }
    };
    let mut attempt = 0u32;
    let res = loop {
        let request = client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .header(ACCEPT_ENCODING, "identity")
            .send();
        let response = match or_cancel(request, cancel).await? {
            Ok(r) => r,
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_SEGMENT_ATTEMPTS {
                    return Err(Error::Http(e));
                }
                let wait = backoff_delay(attempt, fastrand::f64());
                note(RetryCause::of_failed_request(&e), wait);
                sleep_or_cancel(wait, cancel).await?;
                continue;
            }
        };

        let status_code = response.status().as_u16();
        if status_code == 429 || status_code == 503 {
            attempt += 1;
            if attempt >= MAX_SEGMENT_ATTEMPTS {
                return Err(Error::Protocol(format!(
                    "probe received status {status_code} after {MAX_SEGMENT_ATTEMPTS} attempts"
                )));
            }
            let delay = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_secs)
                .unwrap_or_else(|| backoff_delay(attempt, fastrand::f64()));
            note(
                RetryCause::Busy {
                    status: status_code,
                },
                delay,
            );
            sleep_or_cancel(delay, cancel).await?;
            continue;
        }

        break response;
    };

    let status = res.status();
    let status_code = status.as_u16();

    if status_code == 403 || status_code == 404 || status_code == 410 {
        return Err(Error::LinkExpired {
            status: status_code,
        });
    }

    let final_url = res.url().clone();
    let headers = res.headers();

    // Extract strong ETag (must not start with W/ or w/)
    let etag = headers
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.starts_with("W/") && !s.starts_with("w/"));

    let last_modified = headers
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());

    let content_disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());

    let (ranges_supported, total) = match status_code {
        206 => {
            let total = headers
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_total);
            (true, total)
        }
        200 => {
            let total = headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            (false, total)
        }
        416 => (false, None),
        _ if status.is_success() => (false, None),
        code => {
            return Err(Error::Protocol(format!(
                "probe received unexpected status {code}"
            )))
        }
    };

    drop(res);

    Ok(Probe {
        final_url,
        total,
        ranges_supported,
        validator: Validator {
            etag,
            last_modified,
        },
        content_disposition,
    })
}
