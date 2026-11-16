//! Asking GitHub which yt-dlp is newest, and proving what comes back is
//! genuine: the release list gives the version and where the files are, and
//! the signed `SHA2-256SUMS` gives the hash the download must have.

use super::checksum::expected_for;
use super::openpgp::{parse_fingerprint, verify_detached, PinnedKey};
use super::pins::{self, Pin, Platform};
use super::version;
use super::{Tool, ToolError};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const SUMS_ASSET: &str = "SHA2-256SUMS";
const SIG_ASSET: &str = "SHA2-256SUMS.sig";
/// The checksum file and its signature are tiny; anything bigger is not them.
const SUMS_CAP_BYTES: usize = 64 * 1024;
/// A release record with its asset list is a few tens of KiB.
const RELEASE_CAP_BYTES: usize = 1024 * 1024;
/// How long to wait when GitHub says "slow down" without saying for how long.
const DEFAULT_RATE_LIMIT_WAIT_SECS: u64 = 60;

/// Where to look, whom to trust, and what to install. `production()` is the
/// real thing; tests point the URLs at a local server and trust a test key.
#[derive(Clone)]
pub struct Sources {
    /// `https://api.github.com`
    pub api_base: String,
    /// `https://github.com`; release assets must live under it.
    pub download_base: String,
    pub ytdlp_repo: String,
    /// The key that must have signed yt-dlp's checksum file.
    pub key: PinnedKey,
    /// yt-dlp's release asset for this system, if there is one.
    pub ytdlp_asset: Option<String>,
    /// Pinned builds of the other tools for this system.
    pub pins: HashMap<Tool, Pin>,
}

impl Sources {
    pub fn production() -> std::io::Result<Self> {
        let fingerprint = parse_fingerprint(pins::YTDLP_FINGERPRINT)
            .ok_or_else(|| std::io::Error::other("built-in fingerprint is malformed"))?;
        let key = PinnedKey::load(pins::YTDLP_KEY, &fingerprint)
            .map_err(|e| std::io::Error::other(format!("built-in yt-dlp key: {e}")))?;
        let platform = Platform::current();
        let pins = platform
            .map(|platform| {
                [Tool::Ffmpeg, Tool::Deno]
                    .into_iter()
                    .filter_map(|tool| Some((tool, pins::pin_for(tool, platform)?)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            api_base: pins::GITHUB_API.to_string(),
            download_base: pins::GITHUB_DOWNLOADS.to_string(),
            ytdlp_repo: pins::YTDLP_REPO.to_string(),
            key,
            ytdlp_asset: platform.map(|p| p.ytdlp_asset().to_string()),
            pins,
        })
    }
}

/// A verified yt-dlp release, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDlpRelease {
    pub version: String,
    pub asset: String,
    pub url: url::Url,
    pub sha256: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// If a GitHub answer means "limited", the unix time to try again.
/// A 429, or a 403 that carries rate-limit headers, counts.
fn rate_limit_until(status: u16, headers: &reqwest::header::HeaderMap, now: u64) -> Option<u64> {
    let number = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    };
    if let Some(wait) = number("retry-after") {
        return Some(now + wait);
    }
    if number("x-ratelimit-remaining") == Some(0) {
        return Some(number("x-ratelimit-reset").unwrap_or(now + DEFAULT_RATE_LIMIT_WAIT_SECS));
    }
    (status == 429).then_some(now + DEFAULT_RATE_LIMIT_WAIT_SECS)
}

/// GET `url`, reading at most `cap` bytes.
async fn get_capped(
    client: &reqwest::Client,
    tool: Tool,
    url: &str,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| ToolError::network(tool, e))?;
    let status = response.status();
    if !status.is_success() {
        if matches!(status.as_u16(), 403 | 429) {
            if let Some(until) = rate_limit_until(status.as_u16(), response.headers(), now_unix()) {
                return Err(ToolError::RateLimited {
                    tool,
                    until_unix: Some(until),
                    installed: None,
                });
            }
        }
        return Err(if status.is_server_error() {
            ToolError::network(tool, format!("HTTP {status}"))
        } else {
            ToolError::Unusable(format!(
                "GitHub refused the request for {tool} (HTTP {})",
                status.as_u16()
            ))
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ToolError::network(tool, e))?
    {
        if body.len() + chunk.len() > cap {
            return Err(ToolError::Unusable(format!(
                "GitHub's answer for {tool} was larger than expected"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The URL of the release asset called `name`, which must be under the
/// project's own `releases/download/` path on the downloads host.
fn asset_url(sources: &Sources, assets: &[ApiAsset], name: &str) -> Result<url::Url, ToolError> {
    let asset = assets.iter().find(|a| a.name == name).ok_or_else(|| {
        ToolError::Unusable(format!(
            "the latest yt-dlp release has no file called {name}"
        ))
    })?;
    let allowed = format!(
        "{}/{}/releases/download/",
        sources.download_base.trim_end_matches('/'),
        sources.ytdlp_repo
    );
    let url = url::Url::parse(&asset.browser_download_url)
        .ok()
        .filter(|u| u.as_str().starts_with(&allowed))
        .ok_or_else(|| {
            ToolError::Unusable(format!(
                "the yt-dlp release lists {name} outside {allowed}; ignoring it"
            ))
        })?;
    Ok(url)
}

/// Look up the newest yt-dlp release and verify its checksum file against the
/// pinned key. Nothing is downloaded except the release record, `SHA2-256SUMS`
/// and its signature.
pub async fn latest_ytdlp(
    client: &reqwest::Client,
    sources: &Sources,
) -> Result<YtDlpRelease, ToolError> {
    let tool = Tool::YtDlp;
    let wanted_asset = sources
        .ytdlp_asset
        .clone()
        .ok_or_else(|| ToolError::Unusable("yt-dlp isn't available for this system".into()))?;

    let release_url = format!(
        "{}/repos/{}/releases/latest",
        sources.api_base.trim_end_matches('/'),
        sources.ytdlp_repo
    );
    let body = get_capped(client, tool, &release_url, RELEASE_CAP_BYTES).await?;
    let release: ApiRelease = serde_json::from_slice(&body).map_err(|_| {
        ToolError::Unusable("GitHub's release record for yt-dlp is unreadable".into())
    })?;

    // The tag becomes a folder name, so it must be plain dotted numbers.
    if version::parse(&release.tag_name).is_none() {
        return Err(ToolError::Unusable(format!(
            "the latest yt-dlp release has an unexpected version ({})",
            release.tag_name.chars().take(40).collect::<String>()
        )));
    }

    let sums_url = asset_url(sources, &release.assets, SUMS_ASSET)?;
    let sig_url = asset_url(sources, &release.assets, SIG_ASSET)?;
    let url = asset_url(sources, &release.assets, &wanted_asset)?;
    let sums = get_capped(client, tool, sums_url.as_str(), SUMS_CAP_BYTES).await?;
    let sig = get_capped(client, tool, sig_url.as_str(), SUMS_CAP_BYTES).await?;
    verify_detached(&sources.key, &sums, &sig).map_err(|e| ToolError::BadSignature {
        detail: e.to_string(),
    })?;

    let sums_text = String::from_utf8_lossy(&sums);
    let sha256 = expected_for(&sums_text, &wanted_asset).ok_or_else(|| {
        ToolError::Unusable(format!(
            "the signed checksum list for yt-dlp {} doesn't mention {wanted_asset}",
            release.tag_name
        ))
    })?;
    Ok(YtDlpRelease {
        version: release.tag_name,
        asset: wanted_asset,
        url,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn rate_limit_headers_give_the_retry_time() {
        let now = 1_000;
        let reset = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1500"),
        ]);
        assert_eq!(rate_limit_until(403, &reset, now), Some(1500));
        let retry = headers(&[("retry-after", "30")]);
        assert_eq!(rate_limit_until(403, &retry, now), Some(1030));
        assert_eq!(rate_limit_until(429, &HeaderMap::new(), now), Some(1060));
        // A plain 403 is not a rate limit.
        assert_eq!(rate_limit_until(403, &HeaderMap::new(), now), None);
        let plenty = headers(&[("x-ratelimit-remaining", "40")]);
        assert_eq!(rate_limit_until(403, &plenty, now), None);
    }
}
