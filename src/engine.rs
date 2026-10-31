use crate::error::Error;
use crate::probe::{self, Probe};
use crate::ranges::{ByteRange, RangeSet};
use crate::template::{self, Tokens};
use crate::writer::PartFile;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Progress {
    pub downloaded: AtomicU64,
    pub completed: Mutex<RangeSet>,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            completed: Mutex::new(RangeSet::new()),
        }
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DownloadRequest {
    pub url: url::Url,
    pub template: String,
    pub downloads_dir: PathBuf,
    pub connections: u32,
    pub progress: Arc<Progress>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed { path: PathBuf, steals: u32 },
}

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("rustdm/{}", crate::VERSION))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn utc_date_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142400) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn part_path(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", output.display()))
}

fn resolve_output(req: &DownloadRequest, probe: &Probe) -> PathBuf {
    let base_name = template::pick_name(probe.content_disposition.as_deref(), &probe.final_url);
    let host = probe.final_url.host_str().unwrap_or("").to_string();
    let date = utc_date_str();

    let mut current_name = base_name.clone();
    let mut collision_counter = 0;

    loop {
        let tokens = Tokens {
            downloads: req.downloads_dir.to_string_lossy().to_string(),
            host: host.clone(),
            name: current_name.clone(),
            date: date.clone(),
        };

        let expanded = template::expand(&req.template, &tokens);
        let candidate = PathBuf::from(expanded);
        let part = part_path(&candidate);

        if !candidate.exists() && !part.exists() {
            return candidate;
        }

        collision_counter += 1;
        current_name = template::collision_name(&base_name, collision_counter);
    }
}

pub const DEFAULT_CONNECTIONS: u32 = 8;

pub async fn download(client: &reqwest::Client, req: DownloadRequest) -> Result<Outcome, Error> {
    let probe_result = probe::probe(client, &req.url).await?;
    let final_path = resolve_output(&req, &probe_result);
    let part_p = part_path(&final_path);

    let part_file = PartFile::create(&part_p, probe_result.total)?;
    let part_arc = Arc::new(part_file);

    let conns = if req.connections == 0 {
        DEFAULT_CONNECTIONS
    } else {
        req.connections.clamp(1, 16)
    };

    if probe_result.ranges_supported && probe_result.total.is_some_and(|t| t > 0) {
        let total = probe_result.total.unwrap();
        let segments = crate::ranges::initial_segments(total, conns);
        let mut set = tokio::task::JoinSet::new();

        for seg in segments {
            let client_clone = client.clone();
            let url_clone = probe_result.final_url.clone();
            let validator_clone = probe_result.validator.clone();
            let part_clone = Arc::clone(&part_arc);
            let prog_clone = Arc::clone(&req.progress);

            set.spawn(async move {
                crate::worker::fetch_range(
                    &client_clone,
                    &url_clone,
                    &validator_clone,
                    seg,
                    part_clone,
                    prog_clone,
                )
                .await
            });
        }

        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    set.abort_all();
                    return Err(err);
                }
                Err(join_err) => {
                    set.abort_all();
                    return Err(Error::Protocol(join_err.to_string()));
                }
            }
        }
    } else {
        download_single(client, &probe_result, Arc::clone(&part_arc), &req).await?;
    }

    let part_file = Arc::try_unwrap(part_arc)
        .map_err(|_| Error::Protocol("failed to acquire ownership of part file".to_string()))?;

    part_file.finish(&final_path)?;

    Ok(Outcome::Completed {
        path: final_path,
        steals: 0,
    })
}

async fn download_single(
    client: &reqwest::Client,
    probe: &Probe,
    part: Arc<PartFile>,
    req: &DownloadRequest,
) -> Result<(), Error> {
    let mut res = client
        .get(probe.final_url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await?;

    if !res.status().is_success() {
        let status = res.status().as_u16();
        if status == 403 || status == 404 || status == 410 {
            return Err(Error::LinkExpired { status });
        }
        return Err(Error::Protocol(format!(
            "download failed with status {}",
            res.status()
        )));
    }

    let mut offset = 0u64;
    while let Some(chunk) = res.chunk().await? {
        let chunk_len = chunk.len() as u64;
        let part_clone = Arc::clone(&part);
        let curr_offset = offset;
        tokio::task::spawn_blocking(move || part_clone.write_at(curr_offset, &chunk))
            .await
            .map_err(|e| Error::Protocol(e.to_string()))??;

        offset += chunk_len;
        req.progress
            .downloaded
            .fetch_add(chunk_len, Ordering::SeqCst);
        req.progress.completed.lock().unwrap().insert(ByteRange {
            start: curr_offset,
            end: offset,
        });
    }

    if let Some(total) = probe.total {
        if offset != total {
            return Err(Error::Protocol(format!(
                "expected {total} bytes, but received {offset}"
            )));
        }
    }

    Ok(())
}
