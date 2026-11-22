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

/// A direct mp4 or webm stream whose codecs yt-dlp did not report.
fn is_codecless_stream(f: &FormatInfo) -> bool {
    f.vcodec.is_none() && f.acodec.is_none() && (has_ext(f, "mp4") || has_ext(f, "webm"))
}

/// Video and sound in one stream, as far as we can tell.
fn is_whole_stream(f: &FormatInfo) -> bool {
    is_progressive(f) || is_codecless_stream(f)
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
        .filter(|f| is_audio_only(f) && !f.format_id.ends_with("-drc"))
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

/// The best whole stream of a height, or of no known height when `None`.
fn progressive_row(height: Option<u32>, all: &[&FormatInfo]) -> Option<VideoChoice> {
    let candidates = all
        .iter()
        .copied()
        .filter(|f| is_whole_stream(f) && f.height == height && !starts_with(&f.vcodec, &["av01"]));
    let f = best(candidates.filter(|f| has_ext(f, "mp4") || has_ext(f, "webm")))?;
    let container = if has_ext(f, "mp4") {
        Container::Mp4
    } else {
        Container::Webm
    };
    let mut choice = row(height, container, &[(f, StreamKind::Both)]);
    if height.is_none() {
        choice.id = format!("video-{}", container.ext());
        choice.label = "Video".to_string();
    }
    Some(choice)
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
        .filter(|f| is_video_only(f) || is_whole_stream(f))
        .filter_map(|f| f.height)
        .filter(|h| *h > 0)
        .collect();
    let mut rows: Vec<VideoChoice> = heights
        .into_iter()
        .filter_map(|h| {
            split_row(h, &video, aac, opus).or_else(|| progressive_row(Some(h), usable))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.height));
    rows.extend(progressive_row(None, usable));
    rows
}

/// The formats a page has besides its storyboards.
fn real_formats(formats: &[FormatInfo]) -> Vec<&FormatInfo> {
    formats.iter().filter(|f| !is_storyboard(f)).collect()
}

/// Every format that carries video (video-only or progressive) is DRM.
fn all_video_is_drm(formats: &[FormatInfo]) -> bool {
    let video: Vec<&FormatInfo> = real_formats(formats)
        .into_iter()
        .filter(|f| is_video_capable(f))
        .collect();
    !video.is_empty() && video.iter().all(|f| f.is_drm())
}

/// Why no row was left: small pieces only when a stream exists that is not
/// direct and none that is.
fn no_rows_error(formats: &[FormatInfo]) -> ChoicesError {
    let carries_media = |f: &&FormatInfo| has_codec(&f.vcodec) || has_codec(&f.acodec);
    let real = real_formats(formats);
    let direct = real.iter().filter(|f| is_direct(f)).any(carries_media);
    let pieces = real.iter().filter(|f| !is_direct(f)).any(carries_media);
    if pieces && !direct {
        ChoicesError::PiecesOnly
    } else {
        ChoicesError::NoFormats
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
    if all_video_is_drm(&info.formats) {
        return Err(ChoicesError::Drm);
    }
    let usable: Vec<&FormatInfo> = info.formats.iter().filter(|f| usable(f)).collect();
    let mut choices = video_rows(&usable);
    choices.extend(audio_row(&original_audio(&usable)));
    if choices.is_empty() {
        return Err(no_rows_error(&info.formats));
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
        Quality::Audio => rows
            .iter()
            .position(|r| r.id.starts_with("audio-"))
            .unwrap_or(last),
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

    fn codecless(id: &str, ext: &str, height: Option<u32>) -> FormatInfo {
        FormatInfo {
            vcodec: None,
            acodec: None,
            ..fmt(id, ext, "", "", height)
        }
    }

    #[test]
    fn a_codecless_stream_without_a_height_is_one_video_row() {
        let choices = built(vec![codecless("http-1", "mp4", None)]);

        assert_eq!(labels(&choices), ["Video"]);
        let choice = &choices.choices[0];
        assert_eq!(choice.id, "video-mp4");
        assert_eq!(choice.container, Container::Mp4);
        assert_eq!(choice.height, None);
        assert_eq!(choice.streams.len(), 1);
        assert_eq!(choice.streams[0].kind, StreamKind::Both);
    }

    #[test]
    fn a_codecless_stream_with_a_height_is_that_height() {
        let choices = built(vec![codecless("http-1", "mp4", Some(720))]);

        assert_eq!(labels(&choices), ["720p"]);
        assert_eq!(choices.choices[0].streams[0].kind, StreamKind::Both);
    }

    #[test]
    fn the_video_row_comes_after_the_heights_and_before_audio() {
        let mut formats = vec![codecless("a", "mp4", None), codecless("b", "webm", None)];
        formats[1].tbr = Some(5_000.0);
        formats.extend([avc("137", 1080), aac("140", 10)]);

        let choices = built(formats);

        assert_eq!(labels(&choices), ["1080p", "Video", "Audio only"]);
        assert_eq!(choices.choices[1].id, "video-webm");
        assert_eq!(pick(&choices, Quality::Audio), 2);
    }

    #[test]
    fn other_codecless_formats_stay_unused() {
        let formats = vec![
            codecless("x", "flv", Some(480)),
            codecless("y", "m4a", None),
        ];

        assert_eq!(
            build_choices(&page(formats)).unwrap_err(),
            ChoicesError::NoFormats
        );
    }

    #[test]
    fn direct_formats_that_cannot_be_paired_are_no_formats_not_pieces() {
        let formats = vec![avc("137", 1080), vp9("248", 720)];

        assert_eq!(
            build_choices(&page(formats)).unwrap_err(),
            ChoicesError::NoFormats
        );
    }

    #[test]
    fn pieces_beside_unpairable_direct_formats_are_still_no_formats() {
        let formats = vec![avc("137", 1080), pieces("hls", 720)];

        assert_eq!(
            build_choices(&page(formats)).unwrap_err(),
            ChoicesError::NoFormats
        );
    }

    #[test]
    fn clear_audio_does_not_rescue_a_drm_video() {
        let drm_video = FormatInfo {
            has_drm: Some(serde_json::Value::Bool(true)),
            ..avc("137", 1080)
        };

        let result = build_choices(&page(vec![drm_video, aac("140", 10)]));

        assert_eq!(result.unwrap_err(), ChoicesError::Drm);
    }

    #[test]
    fn drc_audio_is_never_chosen() {
        let mut drc = aac("140-drc", 10);
        drc.tbr = Some(999.0);

        let choices = built(vec![avc("137", 1080), aac("140", 10), drc]);

        assert_eq!(stream_ids(row(&choices, "1080p")), ["137", "140"]);
        assert_eq!(stream_ids(row(&choices, "Audio only")), ["140"]);
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
