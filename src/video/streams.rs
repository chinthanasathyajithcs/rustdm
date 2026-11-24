//! The streams a page offers, as the engine will fetch them.

use super::choices::build_choices;
use super::headers::allowed_headers;
use super::info::{FormatInfo, PageInfo};
use super::{StreamKind, StreamPlan};
use std::collections::{BTreeMap, BTreeSet};

/// The most bytes one request may ask for.
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

/// yt-dlp's `http_chunk_size`, kept between 1 byte and 64 MiB.
fn request_bytes(chunk_size: Option<u64>) -> Option<u64> {
    chunk_size.map(|n| n.clamp(1, MAX_REQUEST_BYTES))
}

fn stream_plan(format: &FormatInfo, url: &str, kind: StreamKind) -> StreamPlan {
    StreamPlan {
        format_id: format.format_id.clone(),
        kind,
        url: url.to_string(),
        size: format.filesize.or(format.filesize_approx),
        headers: allowed_headers(&format.http_headers),
        request_bytes: request_bytes(
            format
                .downloader_options
                .as_ref()
                .and_then(|o| o.http_chunk_size),
        ),
        ext: format.ext.clone().unwrap_or_default(),
        file: None,
    }
}

/// One plan for each format that some quality of the page uses, in the page's
/// order. The kind says what the stream carries as the qualities use it.
pub fn stream_plans(info: &PageInfo) -> Vec<StreamPlan> {
    let Ok(choices) = build_choices(info) else {
        return Vec::new();
    };
    let kinds: BTreeMap<&str, StreamKind> = choices
        .choices
        .iter()
        .flat_map(|c| &c.streams)
        .map(|s| (s.format_id.as_str(), s.kind))
        .collect();
    let mut seen = BTreeSet::new();
    info.formats
        .iter()
        .filter_map(|f| {
            let kind = *kinds.get(f.format_id.as_str())?;
            let url = f.url.as_deref()?;
            seen.insert(f.format_id.as_str())
                .then(|| stream_plan(f, url, kind))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::info::DownloaderOptions;

    #[test]
    fn the_chunk_size_is_kept_between_1_byte_and_64_mib() {
        assert_eq!(request_bytes(Some(0)), Some(1));
        assert_eq!(request_bytes(Some(10_485_760)), Some(10_485_760));
        assert_eq!(request_bytes(Some(1 << 40)), Some(67_108_864));
        assert_eq!(request_bytes(None), None);
    }

    fn format(filesize: Option<u64>, approx: Option<u64>) -> FormatInfo {
        FormatInfo {
            format_id: "137".to_string(),
            ext: Some("mp4".to_string()),
            filesize,
            filesize_approx: approx,
            downloader_options: Some(DownloaderOptions {
                http_chunk_size: Some(1 << 40),
            }),
            ..FormatInfo::default()
        }
    }

    #[test]
    fn the_size_is_the_exact_one_else_the_estimate() {
        let exact = stream_plan(
            &format(Some(100), Some(90)),
            "https://a.test/v",
            StreamKind::Video,
        );
        let estimate = stream_plan(
            &format(None, Some(90)),
            "https://a.test/v",
            StreamKind::Video,
        );
        let unknown = stream_plan(&format(None, None), "https://a.test/v", StreamKind::Video);

        assert_eq!(exact.size, Some(100));
        assert_eq!(estimate.size, Some(90));
        assert_eq!(unknown.size, None);
        assert_eq!(exact.request_bytes, Some(MAX_REQUEST_BYTES));
        assert_eq!(exact.ext, "mp4");
        assert_eq!(exact.file, None);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn only_the_formats_some_quality_uses_are_planned() {
        use crate::testing::video::{video_page_json, youtube_like};
        let base = url::Url::parse("https://video.test/").unwrap();
        let page = crate::video::info::parse_page(video_page_json(&youtube_like(&base)).as_bytes())
            .unwrap();

        let plans = stream_plans(&page);

        let ids: Vec<&str> = plans.iter().map(|p| p.format_id.as_str()).collect();
        assert!(ids.contains(&"137") && ids.contains(&"140"));
        assert!(!ids.iter().any(|id| id.starts_with("hls-")));
        assert!(!ids.contains(&"140-3"));
        let audio = plans.iter().find(|p| p.format_id == "140").unwrap();
        assert_eq!(audio.kind, StreamKind::Audio);
        assert_eq!(audio.url, "https://video.test/media/140");
    }
}
