//! The part of `yt-dlp -J` output rustdm reads. Everything is optional or
//! defaulted, and fields we don't use are ignored, so a newer yt-dlp that adds
//! or drops fields does not break the lookup.

use serde::Deserialize;
use std::collections::BTreeMap;

/// A video page (or playlist) as yt-dlp describes it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PageInfo {
    pub id: String,
    pub title: String,
    pub extractor_key: Option<String>,
    pub duration: Option<f64>,
    pub is_live: Option<bool>,
    pub live_status: Option<String>,
    /// `"video"`, `"playlist"` and so on.
    #[serde(rename = "_type")]
    pub kind: Option<String>,
    pub playlist_count: Option<u32>,
    pub entries: Option<Vec<serde_json::Value>>,
    pub webpage_url: Option<String>,
    pub formats: Vec<FormatInfo>,
}

/// One downloadable version of the video: a quality, a codec, a stream.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FormatInfo {
    pub format_id: String,
    pub protocol: Option<String>,
    pub url: Option<String>,
    pub ext: Option<String>,
    /// `"none"` for an audio-only format.
    pub vcodec: Option<String>,
    /// `"none"` for a video-only format.
    pub acodec: Option<String>,
    pub height: Option<u32>,
    pub tbr: Option<f64>,
    pub filesize: Option<u64>,
    pub filesize_approx: Option<u64>,
    pub language_preference: Option<i32>,
    pub format_note: Option<String>,
    /// yt-dlp writes `true`, `false` or `"maybe"`.
    pub has_drm: Option<serde_json::Value>,
    pub http_headers: BTreeMap<String, String>,
    pub downloader_options: Option<DownloaderOptions>,
}

/// How yt-dlp itself would download the format.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DownloaderOptions {
    /// YouTube refuses large ranges; yt-dlp asks for pieces of this size.
    pub http_chunk_size: Option<u64>,
}

impl FormatInfo {
    /// True when the format is copy-protected, or yt-dlp suspects it is.
    pub fn is_drm(&self) -> bool {
        match &self.has_drm {
            Some(serde_json::Value::Bool(drm)) => *drm,
            Some(serde_json::Value::String(text)) => text == "maybe",
            _ => false,
        }
    }
}

/// Read the output of `yt-dlp -J`.
pub fn parse_page(json: &[u8]) -> Result<PageInfo, String> {
    serde_json::from_slice(json).map_err(|e| format!("yt-dlp's answer is not readable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "testing")]
    #[test]
    fn the_fixture_parses_with_formats_and_headers() {
        use crate::testing::video::{video_page_json, youtube_like};
        let base = url::Url::parse("https://video.test/").unwrap();
        let json = video_page_json(&youtube_like(&base));

        let page = parse_page(json.as_bytes()).unwrap();

        assert_eq!(page.title, "A test video");
        assert_eq!(page.extractor_key.as_deref(), Some("Youtube"));
        let h264_1080 = page.formats.iter().find(|f| f.format_id == "137").unwrap();
        assert_eq!(h264_1080.height, Some(1080));
        assert_eq!(h264_1080.vcodec.as_deref(), Some("avc1.640028"));
        assert_eq!(
            h264_1080
                .downloader_options
                .as_ref()
                .and_then(|o| o.http_chunk_size),
            Some(10_485_760)
        );
        assert_eq!(
            h264_1080.http_headers.get("User-Agent").map(String::as_str),
            Some("Mozilla/5.0 (test) rustdm")
        );
        let hls = page
            .formats
            .iter()
            .find(|f| f.format_id == "hls-137")
            .unwrap();
        assert!(hls.downloader_options.is_none());
        assert_eq!(hls.protocol.as_deref(), Some("m3u8_native"));
    }

    #[test]
    fn unknown_fields_and_missing_optional_fields_are_fine() {
        let page = parse_page(br#"{"id":"x","title":"t","something_new":[1,2]}"#).unwrap();

        assert_eq!(page.id, "x");
        assert!(page.formats.is_empty());
        assert_eq!(page.duration, None);
    }

    #[test]
    fn text_that_is_not_json_is_an_error() {
        assert!(parse_page(b"not json").is_err());
    }

    #[test]
    fn has_drm_maybe_counts_as_drm() {
        let drm = |value: &str| {
            let json = format!(r#"{{"format_id":"a","has_drm":{value}}}"#);
            serde_json::from_str::<FormatInfo>(&json).unwrap().is_drm()
        };

        assert!(drm("true"));
        assert!(drm(r#""maybe""#));
        assert!(!drm("false"));
        assert!(!serde_json::from_str::<FormatInfo>(r#"{"format_id":"a"}"#)
            .unwrap()
            .is_drm());
    }
}
