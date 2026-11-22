//! Turns the formats of a page into the quality list the user picks from.

use super::info::{FormatInfo, PageInfo};
use super::{Container, Quality, StreamKind, StreamPick, VideoChoice, VideoChoices};
use std::collections::BTreeSet;

/// The height the list offers first when the page has it.
const DEFAULT_HEIGHT: u32 = 1080;

/// Why a page has no quality to offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoicesError {
    NoFormats,
    PiecesOnly,
    Drm,
}

fn codec(codec: &Option<String>) -> Option<&str> {
    codec.as_deref()
}

fn has_codec(value: &Option<String>) -> bool {
    codec(value).is_some_and(|c| c != "none")
}

fn is_none_codec(value: &Option<String>) -> bool {
    codec(value) == Some("none")
}

fn is_storyboard(f: &FormatInfo) -> bool {
    f.ext.as_deref() == Some("mhtml") || (is_none_codec(&f.vcodec) && is_none_codec(&f.acodec))
}

fn is_video_capable(f: &FormatInfo) -> bool {
    !is_storyboard(f) && has_codec(&f.vcodec)
}

fn is_audio_only(f: &FormatInfo) -> bool {
    is_none_codec(&f.vcodec) && has_codec(&f.acodec)
}

fn is_video_only(f: &FormatInfo) -> bool {
    is_none_codec(&f.acodec) && has_codec(&f.vcodec)
}

fn is_progressive(f: &FormatInfo) -> bool {
    has_codec(&f.vcodec) && has_codec(&f.acodec)
}

fn is_direct(f: &FormatInfo) -> bool {
    matches!(f.protocol.as_deref(), Some("https" | "http"))
}

fn usable(f: &FormatInfo) -> bool {
    is_direct(f) && !is_storyboard(f) && !f.is_drm()
}

fn starts_with(value: &Option<String>, prefix: &[&str]) -> bool {
    codec(value).is_some_and(|c| prefix.iter().any(|p| c.starts_with(p)))
}

fn has_ext(f: &FormatInfo, ext: &str) -> bool {
    f.ext.as_deref() == Some(ext)
}

/// The best of some formats: highest `tbr`, then `filesize`.
fn best<'a>(formats: impl Iterator<Item = &'a FormatInfo>) -> Option<&'a FormatInfo> {
    formats.max_by(|a, b| {
        let by_rate = a.tbr.unwrap_or(0.0).total_cmp(&b.tbr.unwrap_or(0.0));
        by_rate.then(a.filesize.cmp(&b.filesize))
    })
}

/// A dub's id ends in its language: `140-3`, `251-drc-0`.
fn has_language_suffix(f: &FormatInfo) -> bool {
    f.format_id.contains('-')
}

/// The audio-only formats of the original track, never a dub.
fn original_audio<'a>(formats: &[&'a FormatInfo]) -> Vec<&'a FormatInfo> {
    let audio: Vec<&FormatInfo> = formats
        .iter()
        .copied()
        .filter(|f| is_audio_only(f))
        .collect();
    let preference = |f: &FormatInfo| f.language_preference.unwrap_or(0);
    let top = audio.iter().map(|f| preference(f)).max();
    let lowest = audio.iter().map(|f| preference(f)).min();
    if top != lowest {
        return audio
            .into_iter()
            .filter(|f| Some(preference(f)) == top)
            .collect();
    }
    let plain: Vec<&FormatInfo> = audio
        .iter()
        .copied()
        .filter(|f| !has_language_suffix(f))
        .collect();
    if plain.is_empty() {
        audio
    } else {
        plain
    }
}

fn best_aac<'a>(audio: &[&'a FormatInfo]) -> Option<&'a FormatInfo> {
    best(
        audio
            .iter()
            .copied()
            .filter(|f| starts_with(&f.acodec, &["mp4a"]) && has_ext(f, "m4a")),
    )
}

fn best_opus<'a>(audio: &[&'a FormatInfo]) -> Option<&'a FormatInfo> {
    best(audio.iter().copied().filter(|f| has_ext(f, "webm")))
}

fn pick_of(f: &FormatInfo, kind: StreamKind, ext: &str) -> StreamPick {
    StreamPick {
        format_id: f.format_id.clone(),
        kind,
        ext: ext.to_string(),
    }
}

/// Sum the streams' sizes: exact figures, else yt-dlp's estimates; unknown
/// when any stream has neither.
fn total_size(streams: &[&FormatInfo]) -> (Option<u64>, bool) {
    let mut total = 0u64;
    let mut estimate = false;
    for f in streams {
        match (f.filesize, f.filesize_approx) {
            (Some(exact), _) => total += exact,
            (None, Some(approx)) => {
                total += approx;
                estimate = true;
            }
            (None, None) => return (None, false),
        }
    }
    (Some(total), estimate)
}

fn row(
    height: Option<u32>,
    container: Container,
    sources: &[(&FormatInfo, StreamKind)],
) -> VideoChoice {
    let formats: Vec<&FormatInfo> = sources.iter().map(|(f, _)| *f).collect();
    let (size_bytes, size_is_estimate) = total_size(&formats);
    let (id, label) = match height {
        Some(h) => (format!("{h}-{}", container.ext()), format!("{h}p")),
        None => (
            format!("audio-{}", container.ext()),
            "Audio only".to_string(),
        ),
    };
    VideoChoice {
        id,
        label,
        container,
        height,
        size_bytes,
        size_is_estimate,
        streams: sources
            .iter()
            .map(|(f, kind)| pick_of(f, *kind, f.ext.as_deref().unwrap_or_default()))
            .collect(),
    }
}

/// The split row of a height: H.264 with AAC as MP4, else VP9 with Opus as WebM.
fn split_row(
    height: u32,
    video: &[&FormatInfo],
    aac: Option<&FormatInfo>,
    opus: Option<&FormatInfo>,
) -> Option<VideoChoice> {
    let at_height = || video.iter().copied().filter(|f| f.height == Some(height));
    if let (Some(v), Some(a)) = (
        best(at_height().filter(|f| starts_with(&f.vcodec, &["avc1"]) && has_ext(f, "mp4"))),
        aac,
    ) {
        return Some(row(
            Some(height),
            Container::Mp4,
            &[(v, StreamKind::Video), (a, StreamKind::Audio)],
        ));
    }
    let (v, a) = (
        best(
            at_height().filter(|f| starts_with(&f.vcodec, &["vp9", "vp09"]) && has_ext(f, "webm")),
        )?,
        opus?,
    );
    Some(row(
        Some(height),
        Container::Webm,
        &[(v, StreamKind::Video), (a, StreamKind::Audio)],
    ))
}

fn progressive_row(height: u32, all: &[&FormatInfo]) -> Option<VideoChoice> {
    let candidates = all.iter().copied().filter(|f| {
        is_progressive(f) && f.height == Some(height) && !starts_with(&f.vcodec, &["av01"])
    });
    let f = best(candidates.filter(|f| has_ext(f, "mp4") || has_ext(f, "webm")))?;
    let container = if has_ext(f, "mp4") {
        Container::Mp4
    } else {
        Container::Webm
    };
    Some(row(Some(height), container, &[(f, StreamKind::Both)]))
}

fn audio_row(audio: &[&FormatInfo]) -> Option<VideoChoice> {
    if let Some(a) = best_aac(audio) {
        return Some(row(None, Container::M4a, &[(a, StreamKind::Audio)]));
    }
    best_opus(audio).map(|a| row(None, Container::Webm, &[(a, StreamKind::Audio)]))
}

fn video_rows(usable: &[&FormatInfo]) -> Vec<VideoChoice> {
    let audio = original_audio(usable);
    let (aac, opus) = (best_aac(&audio), best_opus(&audio));
    let video: Vec<&FormatInfo> = usable
        .iter()
        .copied()
        .filter(|f| is_video_only(f))
        .collect();
    let heights: BTreeSet<u32> = usable
        .iter()
        .filter(|f| is_video_only(f) || is_progressive(f))
        .filter_map(|f| f.height)
        .filter(|h| *h > 0)
        .collect();
    let mut rows: Vec<VideoChoice> = heights
        .into_iter()
        .filter_map(|h| split_row(h, &video, aac, opus).or_else(|| progressive_row(h, usable)))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.height));
    rows
}

/// Why no row was left, for a page that offers nothing.
fn failure(formats: &[FormatInfo]) -> ChoicesError {
    let real: Vec<&FormatInfo> = formats.iter().filter(|f| !is_storyboard(f)).collect();
    let video: Vec<&&FormatInfo> = real.iter().filter(|f| is_video_capable(f)).collect();
    if real.is_empty() {
        ChoicesError::NoFormats
    } else if !video.is_empty() && video.iter().all(|f| f.is_drm()) {
        ChoicesError::Drm
    } else {
        ChoicesError::PiecesOnly
    }
}

fn default_index(rows: &[VideoChoice]) -> usize {
    let videos = || rows.iter().enumerate().filter(|(_, r)| r.height.is_some());
    videos()
        .find(|(_, r)| r.height == Some(DEFAULT_HEIGHT))
        .or_else(|| videos().find(|(_, r)| r.height < Some(DEFAULT_HEIGHT)))
        .or_else(|| videos().next_back())
        .map_or(0, |(i, _)| i)
}

/// The quality list of a page, highest first with "Audio only" last.
pub fn build_choices(info: &PageInfo) -> Result<VideoChoices, ChoicesError> {
    let usable: Vec<&FormatInfo> = info.formats.iter().filter(|f| usable(f)).collect();
    let mut choices = video_rows(&usable);
    choices.extend(audio_row(&original_audio(&usable)));
    if choices.is_empty() {
        return Err(failure(&info.formats));
    }
    let default = default_index(&choices);
    Ok(VideoChoices {
        title: info.title.clone(),
        duration_secs: info.duration.map(|d| d.round() as u64),
        choices,
        default,
    })
}

/// The index of the row that best matches a quality asked for.
pub fn pick(choices: &VideoChoices, quality: Quality) -> usize {
    let rows = &choices.choices;
    let last = rows.len().saturating_sub(1);
    match quality {
        Quality::Best => 0,
        Quality::Audio => rows.iter().position(|r| r.height.is_none()).unwrap_or(last),
        Quality::Height(wanted) => {
            let videos = || rows.iter().enumerate().filter(|(_, r)| r.height.is_some());
            videos()
                .find(|(_, r)| r.height == Some(wanted))
                .or_else(|| videos().find(|(_, r)| r.height < Some(wanted)))
                .or_else(|| videos().next_back())
                .map_or(0, |(i, _)| i)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(id: &str, ext: &str, vcodec: &str, acodec: &str, height: Option<u32>) -> FormatInfo {
        FormatInfo {
            format_id: id.to_string(),
            protocol: Some("https".to_string()),
            url: Some(format!("https://video.test/{id}")),
            ext: Some(ext.to_string()),
            vcodec: Some(vcodec.to_string()),
            acodec: Some(acodec.to_string()),
            height,
            tbr: Some(height.unwrap_or(100) as f64),
            filesize: Some(u64::from(height.unwrap_or(1)) * 1_000),
            ..FormatInfo::default()
        }
    }

    fn avc(id: &str, height: u32) -> FormatInfo {
        fmt(id, "mp4", "avc1.640028", "none", Some(height))
    }

    fn vp9(id: &str, height: u32) -> FormatInfo {
        fmt(id, "webm", "vp09.00.40.08", "none", Some(height))
    }

    fn aac(id: &str, preference: i32) -> FormatInfo {
        FormatInfo {
            language_preference: Some(preference),
            ..fmt(id, "m4a", "none", "mp4a.40.2", None)
        }
    }

    fn opus(id: &str, preference: i32) -> FormatInfo {
        FormatInfo {
            language_preference: Some(preference),
            ..fmt(id, "webm", "none", "opus", None)
        }
    }

    fn pieces(id: &str, height: u32) -> FormatInfo {
        FormatInfo {
            protocol: Some("m3u8_native".to_string()),
            ..fmt(id, "mp4", "avc1.640028", "none", Some(height))
        }
    }

    fn page(formats: Vec<FormatInfo>) -> PageInfo {
        PageInfo {
            title: "A test video".to_string(),
            duration: Some(61.6),
            formats,
            ..PageInfo::default()
        }
    }

    /// 2160 and 1440 are VP9 only; 1080 to 360 are H.264 (1080 also AV1).
    fn youtube_like() -> Vec<FormatInfo> {
        let mut formats = vec![vp9("313", 2160), vp9("271", 1440)];
        formats.push(avc("137", 1080));
        formats.push(avc("136", 720));
        formats.push(avc("135", 480));
        formats.push(avc("134", 360));
        formats.push(vp9("248", 1080));
        formats.push(fmt("399", "mp4", "av01.0.08M.08", "none", Some(1080)));
        formats.push(pieces("137-hls", 1080));
        formats.push(FormatInfo {
            protocol: Some("mhtml".to_string()),
            ..fmt("sb0", "mhtml", "none", "none", Some(45))
        });
        for (id, preference) in [("140", 10), ("140-0", -1), ("140-3", -1)] {
            formats.push(aac(id, preference));
        }
        for (id, preference) in [("251", 10), ("251-3", -1)] {
            formats.push(opus(id, preference));
        }
        formats
    }

    fn labels(choices: &VideoChoices) -> Vec<&str> {
        choices.choices.iter().map(|c| c.label.as_str()).collect()
    }

    fn built(formats: Vec<FormatInfo>) -> VideoChoices {
        build_choices(&page(formats)).unwrap()
    }

    fn row<'a>(choices: &'a VideoChoices, label: &str) -> &'a VideoChoice {
        choices.choices.iter().find(|c| c.label == label).unwrap()
    }

    fn stream_ids(choice: &VideoChoice) -> Vec<&str> {
        choice
            .streams
            .iter()
            .map(|s| s.format_id.as_str())
            .collect()
    }

    #[test]
    fn qualities_run_from_highest_to_lowest_with_audio_last() {
        let choices = built(youtube_like());

        assert_eq!(
            labels(&choices),
            [
                "2160p",
                "1440p",
                "1080p",
                "720p",
                "480p",
                "360p",
                "Audio only"
            ]
        );
        assert_eq!(choices.title, "A test video");
        assert_eq!(choices.duration_secs, Some(62));
    }

    #[test]
    fn the_default_is_1080p() {
        let choices = built(youtube_like());

        assert_eq!(choices.choices[choices.default].label, "1080p");
    }

    #[test]
    fn without_1080p_the_default_is_the_highest_below() {
        let without = |heights: &[u32]| -> Vec<FormatInfo> {
            youtube_like()
                .into_iter()
                .filter(|f| f.height.is_none_or(|h| heights.contains(&h)) || f.height == Some(45))
                .collect()
        };

        let below = built(without(&[2160, 1440, 720, 480, 360]));
        let above = built(without(&[2160, 1440]));

        assert_eq!(below.choices[below.default].label, "720p");
        assert_eq!(above.choices[above.default].label, "1440p");
    }

    #[test]
    fn mp4_rows_pair_h264_with_original_aac() {
        let choices = built(youtube_like());
        let choice = row(&choices, "1080p");

        assert_eq!(choice.id, "1080-mp4");
        assert_eq!(choice.container, Container::Mp4);
        assert_eq!(
            choice.streams,
            vec![
                StreamPick {
                    format_id: "137".to_string(),
                    kind: StreamKind::Video,
                    ext: "mp4".to_string()
                },
                StreamPick {
                    format_id: "140".to_string(),
                    kind: StreamKind::Audio,
                    ext: "m4a".to_string()
                },
            ]
        );
    }

    #[test]
    fn heights_without_h264_are_webm_with_original_opus() {
        let choices = built(youtube_like());
        let choice = row(&choices, "1440p");

        assert_eq!(choice.id, "1440-webm");
        assert_eq!(choice.container, Container::Webm);
        assert_eq!(stream_ids(choice), ["271", "251"]);
    }

    #[test]
    fn dubbed_audio_is_never_chosen() {
        let choices = built(youtube_like());
        let used: Vec<&str> = choices.choices.iter().flat_map(stream_ids).collect();

        for dub in ["140-0", "140-3", "251-3"] {
            assert!(!used.contains(&dub), "{dub} was chosen");
        }
        let only_dubs = built(vec![avc("137", 1080), aac("140-0", -1), aac("140-3", -1)]);
        assert!(stream_ids(row(&only_dubs, "1080p"))[1].starts_with("140-"));
    }

    #[test]
    fn av1_and_piece_streams_are_left_out() {
        let choices = built(youtube_like());
        let used: Vec<&str> = choices.choices.iter().flat_map(stream_ids).collect();

        assert!(!used.contains(&"399"));
        assert!(!used.contains(&"137-hls"));
        assert!(!used.contains(&"sb0"));
    }

    #[test]
    fn audio_only_is_the_original_aac_else_opus() {
        let aac_row = built(youtube_like());
        let opus_row = built(vec![avc("137", 1080), opus("251", 10)]);

        assert_eq!(row(&aac_row, "Audio only").id, "audio-m4a");
        assert_eq!(row(&aac_row, "Audio only").container, Container::M4a);
        assert_eq!(row(&opus_row, "Audio only").id, "audio-webm");
        assert_eq!(row(&opus_row, "Audio only").container, Container::Webm);
        assert_eq!(labels(&opus_row), ["Audio only"]);
    }

    #[test]
    fn a_progressive_format_is_used_only_when_no_pair_exists() {
        let progressive = || fmt("18", "mp4", "avc1.42001E", "mp4a.40.2", Some(360));

        let alone = built(vec![progressive()]);
        let with_pair = built(vec![progressive(), avc("134", 360), aac("140", 10)]);

        assert_eq!(labels(&alone), ["360p"]);
        assert_eq!(alone.choices[0].streams.len(), 1);
        assert_eq!(alone.choices[0].streams[0].kind, StreamKind::Both);
        assert_eq!(stream_ids(&with_pair.choices[0]), ["134", "140"]);
    }

    #[test]
    fn video_only_heights_without_any_audio_are_not_offered() {
        let progressive = fmt("18", "mp4", "avc1.42001E", "mp4a.40.2", Some(360));

        let choices = built(vec![avc("137", 1080), progressive]);

        assert_eq!(labels(&choices), ["360p"]);
    }

    #[test]
    fn sizes_add_up_and_estimates_are_marked() {
        let mut formats = youtube_like();
        for f in formats.iter_mut().filter(|f| f.format_id == "140") {
            f.filesize = None;
            f.filesize_approx = Some(500);
        }
        formats
            .iter_mut()
            .find(|f| f.format_id == "271")
            .unwrap()
            .filesize = None;

        let choices = built(formats);

        assert_eq!(row(&choices, "1080p").size_bytes, Some(1_080_000 + 500));
        assert!(row(&choices, "1080p").size_is_estimate);
        assert_eq!(row(&choices, "1440p").size_bytes, None);
        assert!(!row(&choices, "1440p").size_is_estimate);
        assert!(row(&choices, "Audio only").size_is_estimate);
        let exact = built(youtube_like());
        assert_eq!(row(&exact, "720p").size_bytes, Some(720_000 + 1_000));
        assert!(!row(&exact, "720p").size_is_estimate);
    }

    #[test]
    fn only_piece_streams_is_pieces_only() {
        let formats = vec![pieces("hls-1080", 1080), pieces("hls-720", 720)];

        assert_eq!(
            build_choices(&page(formats)).unwrap_err(),
            ChoicesError::PiecesOnly
        );
    }

    #[test]
    fn no_formats_is_no_formats() {
        let storyboard = fmt("sb0", "mhtml", "none", "none", Some(45));

        assert_eq!(
            build_choices(&page(vec![])).unwrap_err(),
            ChoicesError::NoFormats
        );
        assert_eq!(
            build_choices(&page(vec![storyboard])).unwrap_err(),
            ChoicesError::NoFormats
        );
    }

    #[test]
    fn every_video_format_with_drm_is_drm() {
        let drm = |f: FormatInfo| FormatInfo {
            has_drm: Some(serde_json::Value::Bool(true)),
            ..f
        };
        let formats = vec![
            drm(avc("137", 1080)),
            drm(avc("136", 720)),
            drm(pieces("hls", 360)),
        ];

        assert_eq!(
            build_choices(&page(formats)).unwrap_err(),
            ChoicesError::Drm
        );
    }

    #[test]
    fn pick_follows_the_quality_asked() {
        let choices = built(youtube_like());
        let label = |q| choices.choices[pick(&choices, q)].label.as_str();

        assert_eq!(label(Quality::Best), "2160p");
        assert_eq!(label(Quality::Audio), "Audio only");
        assert_eq!(label(Quality::Height(1080)), "1080p");
        assert_eq!(label(Quality::Height(900)), "720p");
        assert_eq!(label(Quality::Height(100)), "360p");
    }
}
