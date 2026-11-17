//! Asking GitHub which yt-dlp is newest, and proving what comes back is
//! genuine: the release list gives the version and where the files are, and
//! the signed `SHA2-256SUMS` gives the hash the download must have.

use super::checksum::{parse_strict_sums, unique_hash};
use super::openpgp::{parse_fingerprint, verify_detached_any, PinnedKey};
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
/// The longest we ever hold off asking again, whatever GitHub's headers say.
pub const MAX_RATE_LIMIT_WAIT_SECS: u64 = 60 * 60;

/// Where to look, whom to trust, and what to install. `production()` is the
/// real thing; tests point the URLs at a local server and trust a test key.
#[derive(Clone)]
pub struct Sources {
    /// `https://api.github.com`
    pub api_base: String,
    /// `https://github.com`; release assets must live under it.
    pub download_base: String,
    pub ytdlp_repo: String,
    /// The keys one of which must have signed yt-dlp's checksum file.
    pub keys: Vec<PinnedKey>,
    /// The oldest yt-dlp version that may be installed.
    pub ytdlp_min_version: String,
    /// yt-dlp's release asset for this system, if there is one.
    pub ytdlp_asset: Option<String>,
    /// Pinned builds of the other tools for this system.
    pub pins: HashMap<Tool, Pin>,
}

impl Sources {
    pub fn production() -> std::io::Result<Self> {
        let keys = pins::YTDLP_KEYS
            .iter()
            .map(|(fingerprint, key)| {
                let fingerprint = parse_fingerprint(fingerprint)
                    .ok_or_else(|| std::io::Error::other("built-in fingerprint is malformed"))?;
                PinnedKey::load(key, &fingerprint)
                    .map_err(|e| std::io::Error::other(format!("built-in yt-dlp key: {e}")))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
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
            keys,
            ytdlp_min_version: pins::YTDLP_MIN_VERSION.to_string(),
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
    /// When yt-dlp's key signed the checksum file (unix seconds), from the
    /// signature itself rather than from GitHub.
    pub signed_at: u64,
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

/// If a GitHub answer means "limited", the unix time to try again, never more
/// than an hour away. A 429, or a 403 that carries rate-limit headers, counts.
fn rate_limit_until(status: u16, headers: &reqwest::header::HeaderMap, now: u64) -> Option<u64> {
    let number = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    };
    let until = if let Some(wait) = number("retry-after") {
        now.saturating_add(wait)
    } else if number("x-ratelimit-remaining") == Some(0) {
        number("x-ratelimit-reset")
            .unwrap_or_else(|| now.saturating_add(DEFAULT_RATE_LIMIT_WAIT_SECS))
    } else if status == 429 {
        now.saturating_add(DEFAULT_RATE_LIMIT_WAIT_SECS)
    } else {
        return None;
    };
    Some(until.min(now.saturating_add(MAX_RATE_LIMIT_WAIT_SECS)))
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

/// The release record GitHub gives for the newest yt-dlp, with a tag that is
/// safe to use as a folder name.
async fn release_record(
    client: &reqwest::Client,
    sources: &Sources,
) -> Result<ApiRelease, ToolError> {
    let release_url = format!(
        "{}/repos/{}/releases/latest",
        sources.api_base.trim_end_matches('/'),
        sources.ytdlp_repo
    );
    let body = get_capped(client, Tool::YtDlp, &release_url, RELEASE_CAP_BYTES).await?;
    let release: ApiRelease = serde_json::from_slice(&body).map_err(|_| {
        ToolError::Unusable("GitHub's release record for yt-dlp is unreadable".into())
    })?;

    // The tag becomes a folder name, so it must be plain dotted numbers.
    let Some(parsed) = version::parse(&release.tag_name) else {
        return Err(ToolError::Unusable(format!(
            "the latest yt-dlp release has an unexpected version ({})",
            release.tag_name.chars().take(40).collect::<String>()
        )));
    };
    // The tag is unsigned, so an old release could be offered under a new tag; it
    // is checked again against what the program itself reports. This floor
    // refuses anything older than the oldest release rustdm has seen.
    let floor = version::parse(&sources.ytdlp_min_version).unwrap_or_default();
    if version::compare(&parsed, &floor) == std::cmp::Ordering::Less {
        return Err(ToolError::Unusable(format!(
            "yt-dlp {} is older than the oldest version rustdm accepts ({}); ignoring it",
            release.tag_name, sources.ytdlp_min_version
        )));
    }
    Ok(release)
}

/// Fetch `SHA2-256SUMS` and its signature and check them against the pinned
/// keys. Returns the file's text and when it was signed.
async fn signed_sums(
    client: &reqwest::Client,
    sources: &Sources,
    assets: &[ApiAsset],
) -> Result<(String, u64), ToolError> {
    let tool = Tool::YtDlp;
    let sums_url = asset_url(sources, assets, SUMS_ASSET)?;
    let sig_url = asset_url(sources, assets, SIG_ASSET)?;
    let sums = get_capped(client, tool, sums_url.as_str(), SUMS_CAP_BYTES).await?;
    let sig = get_capped(client, tool, sig_url.as_str(), SUMS_CAP_BYTES).await?;
    let verified =
        verify_detached_any(&sources.keys, &sums, &sig).map_err(|e| ToolError::BadSignature {
            detail: e.to_string(),
        })?;
    let text = String::from_utf8(sums).map_err(|_| {
        ToolError::Unusable("the signed checksum list for yt-dlp isn't text".into())
    })?;
    Ok((text, verified.created_unix))
}

/// Look up the newest yt-dlp release and verify its checksum file against the
/// pinned keys. Nothing is downloaded except the release record, `SHA2-256SUMS`
/// and its signature.
pub async fn latest_ytdlp(
    client: &reqwest::Client,
    sources: &Sources,
) -> Result<YtDlpRelease, ToolError> {
    let wanted_asset = sources
        .ytdlp_asset
        .clone()
        .ok_or_else(|| ToolError::Unusable("yt-dlp isn't available for this system".into()))?;
    let release = release_record(client, sources).await?;
    let url = asset_url(sources, &release.assets, &wanted_asset)?;
    let (sums_text, signed_at) = signed_sums(client, sources, &release.assets).await?;

    let lines = parse_strict_sums(&sums_text).ok_or_else(|| {
        ToolError::Unusable(format!(
            "the signed checksum list for yt-dlp {} isn't in the expected format",
            release.tag_name
        ))
    })?;
    let sha256 = unique_hash(&lines, &wanted_asset).ok_or_else(|| {
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
        signed_at,
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

    #[test]
    fn the_wait_is_capped_at_an_hour_and_cannot_overflow() {
        let now = 1_000;
        let far = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "99999999999"),
        ]);
        assert_eq!(rate_limit_until(403, &far, now), Some(now + 3600));
        let forever = headers(&[("retry-after", "18446744073709551615")]);
        assert_eq!(rate_limit_until(429, &forever, now), Some(now + 3600));
        assert_eq!(rate_limit_until(429, &forever, u64::MAX), Some(u64::MAX));
    }
}
