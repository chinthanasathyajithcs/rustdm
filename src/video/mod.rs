//! Video downloads: the types shared by the lookup, the quality list and the
//! stored plan of a video download.

pub mod choices;
pub mod dash;
pub mod errors;
pub mod headers;
pub mod hls;
pub mod info;
pub mod join;
pub mod refresh;
pub mod streams;
pub mod ytdlp;

use info::PageInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

/// The file type a quality is saved as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Container {
    Mp4,
    Webm,
    M4a,
}

impl Container {
    /// The file name ending, without the dot.
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Webm => "webm",
            Container::M4a => "m4a",
        }
    }

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4",
            Container::Webm => "WebM",
            Container::M4a => "M4A",
        }
    }
}

/// What a single stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    Video,
    Audio,
    /// Video and sound in one stream.
    Both,
}

/// A yt-dlp format picked as one stream of a quality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPick {
    pub format_id: String,
    pub kind: StreamKind,
    pub ext: String,
}

/// One row of the quality list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoChoice {
    /// Stable within one lookup: "1080-mp4", "audio-m4a".
    pub id: String,
    /// "1080p", "Audio only".
    pub label: String,
    pub container: Container,
    pub height: Option<u32>,
    pub size_bytes: Option<u64>,
    /// True when the size comes from yt-dlp's estimate, not an exact figure.
    pub size_is_estimate: bool,
    /// One or two streams.
    pub streams: Vec<StreamPick>,
}

/// The answer to a lookup: the video and the qualities it can be saved in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoChoices {
    pub title: String,
    pub duration_secs: Option<u64>,
    pub choices: Vec<VideoChoice>,
    /// Index into `choices` of the row to offer first.
    pub default: usize,
}

/// A quality asked for on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Best,
    Audio,
    Height(u32),
}

const QUALITY_ERROR: &str = "quality must be best, audio or a height such as 1080p";

/// Read `best`, `audio`, `1080p` or `1080` (any letter case).
pub fn parse_quality(s: &str) -> Result<Quality, String> {
    let text = s.trim().to_ascii_lowercase();
    match text.as_str() {
        "best" => return Ok(Quality::Best),
        "audio" => return Ok(Quality::Audio),
        _ => {}
    }
    let digits = text.strip_suffix('p').unwrap_or(&text);
    match digits.parse::<u32>() {
        Ok(height) if height > 0 => Ok(Quality::Height(height)),
        _ => Err(QUALITY_ERROR.to_string()),
    }
}

/// The quality the user chose, kept with the download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenQuality {
    pub label: String,
    pub container: Container,
    /// The `VideoChoice::id` it was chosen by.
    pub choice_id: String,
}

/// Where a video download is in its work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoStep {
    /// Waiting for the user to pick a quality.
    Choosing,
    /// Downloading the stream at this index of `VideoPlan::streams`.
    Stream(usize),
    /// Joining the streams into one file.
    Join,
    Done,
}

/// One stream of a chosen quality, with what the engine needs to fetch it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPlan {
    pub format_id: String,
    pub kind: StreamKind,
    /// The stream file's ending: `mp4`, `m4a`, `webm`.
    #[serde(default)]
    pub ext: String,
    /// Expires; refreshed when the server refuses it.
    pub url: String,
    pub size: Option<u64>,
    /// Already filtered by the allowlist.
    pub headers: Vec<(String, String)>,
    /// yt-dlp's `http_chunk_size`, capped to 1..=64 MiB.
    pub request_bytes: Option<u64>,
    /// The stream's file, once the engine has named it.
    pub file: Option<PathBuf>,
    /// How the stream is fetched; streams saved before pieces mode are direct.
    #[serde(default)]
    pub source: StreamSource,
}

/// How a stream is fetched.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum StreamSource {
    /// One link, through the range engine.
    #[default]
    Direct,
    /// An HLS media playlist, read again on each run.
    Hls { playlist: String },
    /// DASH pieces in order, the init piece first.
    Dash { pieces: Vec<DashPiece> },
}

/// One DASH piece: its link, and how long it plays (its fingerprint part).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashPiece {
    pub link: String,
    #[serde(default)]
    pub duration_ms: u64,
}

impl StreamSource {
    pub fn is_pieces(&self) -> bool {
        !matches!(self, StreamSource::Direct)
    }
}

/// Everything stored with a video download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPlan {
    pub page_url: String,
    pub title: String,
    /// How long the video runs, when the site says.
    #[serde(default)]
    pub duration_secs: Option<u64>,
    /// What was chosen; none while the plan waits for a choice.
    #[serde(default)]
    pub choice: Option<ChosenQuality>,
    /// Kept only while choosing; emptied after the choice.
    pub choices: Vec<VideoChoice>,
    /// Every stream the choices use; kept only while choosing, emptied by the choice.
    #[serde(default)]
    pub offered: Vec<StreamPlan>,
    /// Index into `choices` of the row to offer first.
    #[serde(default)]
    pub default: usize,
    /// Filled after the choice.
    pub streams: Vec<StreamPlan>,
    pub step: VideoStep,
    /// Unix seconds of recent link refreshes.
    pub refreshes: Vec<u64>,
    /// The site's name for messages (`site_name`); empty in plans saved by 1a.
    #[serde(default)]
    pub site: String,
    /// Where the plan came from; plans saved before 1c are pages.
    #[serde(default)]
    pub origin: PlanOrigin,
}

/// Where a video plan came from, which says how its links are refreshed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanOrigin {
    /// A page looked up by yt-dlp.
    #[default]
    Page,
    /// A direct `.m3u8` link, read by rustdm.
    Playlist,
}

/// The site's name for messages: yt-dlp's extractor name (any `Youtube...`
/// shown as `YouTube`), else the page's host without `www.`. `Generic` is
/// yt-dlp's catch-all for unknown sites, so it counts as no name.
pub fn site_name(info: &PageInfo, page_url: &Url) -> String {
    match info.extractor_key.as_deref() {
        Some(key) if key.starts_with("Youtube") => "YouTube".to_string(),
        Some(key) if !key.is_empty() && key != "Generic" => key.to_string(),
        _ => {
            let host = page_url.host_str().unwrap_or_default();
            host.strip_prefix("www.").unwrap_or(host).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn quality_values_parse() {
        assert_eq!(parse_quality("best"), Ok(Quality::Best));
        assert_eq!(parse_quality("AUDIO"), Ok(Quality::Audio));
        assert_eq!(parse_quality("1080p"), Ok(Quality::Height(1080)));
        assert_eq!(parse_quality("720"), Ok(Quality::Height(720)));
        for bad in ["", "1080i", "0p", "hd"] {
            assert_eq!(
                parse_quality(bad),
                Err("quality must be best, audio or a height such as 1080p".to_string()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn site_name_prefers_the_extractor() {
        let url = Url::parse("https://www.example.org/v").unwrap();
        let named = |key: Option<&str>| PageInfo {
            extractor_key: key.map(String::from),
            ..PageInfo::default()
        };

        assert_eq!(site_name(&named(Some("Youtube")), &url), "YouTube");
        assert_eq!(site_name(&named(Some("Vimeo")), &url), "Vimeo");
        assert_eq!(site_name(&named(None), &url), "example.org");
        assert_eq!(site_name(&named(Some("YoutubeTab")), &url), "YouTube");
        assert_eq!(site_name(&named(Some("Generic")), &url), "example.org");
    }

    #[test]
    fn container_names_and_extensions() {
        assert_eq!(Container::Mp4.ext(), "mp4");
        assert_eq!(Container::Webm.label(), "WebM");
        assert_eq!(Container::M4a.ext(), "m4a");
        assert_eq!(Container::M4a.label(), "M4A");
    }

    #[test]
    fn a_video_plan_survives_json() {
        let choice = VideoChoice {
            id: "1080-mp4".to_string(),
            label: "1080p".to_string(),
            container: Container::Mp4,
            height: Some(1080),
            size_bytes: Some(1_000),
            size_is_estimate: true,
            streams: vec![StreamPick {
                format_id: "137".to_string(),
                kind: StreamKind::Video,
                ext: "mp4".to_string(),
            }],
        };
        let plan = VideoPlan {
            page_url: "https://video.test/watch?v=1".to_string(),
            title: "A test video".to_string(),
            duration_secs: Some(149),
            choice: Some(ChosenQuality {
                label: "1080p".to_string(),
                container: Container::Mp4,
                choice_id: "1080-mp4".to_string(),
            }),
            choices: vec![choice],
            offered: Vec::new(),
            default: 0,
            streams: vec![StreamPlan {
                format_id: "137".to_string(),
                kind: StreamKind::Video,
                ext: "mp4".to_string(),
                url: "https://video.test/media/137".to_string(),
                size: Some(1_000),
                headers: vec![("User-Agent".to_string(), "x".to_string())],
                request_bytes: Some(10_485_760),
                file: Some(std::path::PathBuf::from("a.part")),
                source: StreamSource::Hls {
                    playlist: "https://video.test/v.m3u8".to_string(),
                },
            }],
            step: VideoStep::Stream(0),
            refreshes: vec![1_700_000_000],
            site: "YouTube".to_string(),
            origin: crate::video::PlanOrigin::Page,
        };

        let json = serde_json::to_string(&plan).unwrap();
        let back: VideoPlan = serde_json::from_str(&json).unwrap();

        assert_eq!(back, plan);
        assert!(json.contains(r#""container":"mp4""#));
        assert!(json.contains(r#""kind":"video""#));
    }

    #[test]
    fn a_stream_saved_before_pieces_mode_is_direct() {
        let json = r#"{"format_id":"137","kind":"video","ext":"mp4","url":"u","size":1,
            "headers":[],"request_bytes":null,"file":null}"#;

        let stream: StreamPlan = serde_json::from_str(json).unwrap();

        assert_eq!(stream.source, StreamSource::Direct);
    }

    #[test]
    fn a_plan_before_any_choice_has_no_chosen_quality() {
        let json = r#"{"page_url":"u","title":"t","choices":[],"streams":[],
            "step":"choosing","refreshes":[]}"#;
        let plan: VideoPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.choice, None);
        assert_eq!(plan.default, 0);
        assert_eq!(plan.step, VideoStep::Choosing);
    }
}
