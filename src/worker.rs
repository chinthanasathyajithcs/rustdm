use crate::engine::Progress;
use crate::error::Error;
use crate::probe::Validator;
use crate::ranges::ByteRange;
use crate::writer::PartFile;
use reqwest::header::{ACCEPT_ENCODING, IF_RANGE, RANGE};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub async fn fetch_range(
    client: &reqwest::Client,
    url: &url::Url,
    validator: &Validator,
    range: ByteRange,
    part: Arc<PartFile>,
    progress: Arc<Progress>,
) -> Result<(), Error> {
    if range.is_empty() {
        return Ok(());
    }

    let mut req_builder = client
        .get(url.clone())
        .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1))
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

    let mut res = req_builder.send().await?;
    let status = res.status();
    let status_code = status.as_u16();

    if status_code == 200 && has_if_range {
        return Err(Error::FileChanged);
    }

    if status_code == 403 || status_code == 404 || status_code == 410 {
        return Err(Error::LinkExpired {
            status: status_code,
        });
    }

    if status_code != 206 {
        return Err(Error::Protocol(format!(
            "expected 206 Partial Content, received status {status_code}"
        )));
    }

    let mut offset = range.start;
    while let Some(chunk) = res.chunk().await? {
        let chunk_len = chunk.len() as u64;
        let part_clone = Arc::clone(&part);
        let curr_offset = offset;

        tokio::task::spawn_blocking(move || part_clone.write_at(curr_offset, &chunk))
            .await
            .map_err(|e| Error::Protocol(e.to_string()))??;

        offset += chunk_len;
        progress.downloaded.fetch_add(chunk_len, Ordering::SeqCst);
        progress.completed.lock().unwrap().insert(ByteRange {
            start: curr_offset,
            end: offset,
        });
    }

    if offset != range.end {
        return Err(Error::Protocol(format!(
            "range underflow: expected {} bytes, got {}",
            range.len(),
            offset - range.start
        )));
    }

    Ok(())
}
