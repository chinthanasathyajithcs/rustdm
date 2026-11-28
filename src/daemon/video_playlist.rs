//! A direct `.m3u8` link: its playlist is read by rustdm (no yt-dlp) and
//! turned into the same page and quality list a lookup gives (spec 1c, section 6).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_pieces::{fetch_playlist, unreadable};
use super::video_task::LookupOutcome;
use crate::engine::{Outcome, Timeouts};
use crate::error::Error;
use crate::request_context::{stream_client, RequestContext};
use crate::video::choices::{build_choices, ChoicesError};
use crate::video::errors::{DRM, LIVE};
use crate::video::hls::{parse_playlist, AudioRendition, HlsError, Master, Media, Playlist};
use crate::video::info::{FormatInfo, PageInfo};
use crate::video::PlanOrigin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use url::Url;

/// Link names that say nothing about the video.
const GENERIC_NAMES: [&str; 8] = [
    "master",
    "index",
    "playlist",
    "prog_index",
    "chunklist",
    "manifest",
    "main",
    "media",
];
const VIDEO_CODECS: [&str; 7] = ["avc1", "avc3", "hvc1", "hev1", "vp09", "vp9", "av01"];
const AUDIO_CODECS: [&str; 5] = ["mp4a", "opus", "ac-3", "ec-3", "flac"];
const HLS: &str = "m3u8_native";

impl DaemonCore {
    /// The engine found an HLS playlist: on a fresh download it is read as a
    /// video instead of being saved. Any other outcome is returned to be
    /// settled as before; `None` means the playlist settled the download.
    pub(super) async fn playlist_to_video(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        outcome: Result<Outcome, Error>,
        cancel: watch::Receiver<bool>,
        fresh: bool,
    ) -> Option<Result<Outcome, Error>> {
        if !matches!(outcome, Err(Error::Playlist)) || !fresh {
            return Some(outcome);
        }
        let wanted = {
            let rec = record.lock().unwrap();
            rec.status == crate::ipc::protocol::DownloadStatus::Active && rec.video.is_none()
        };
        if !wanted {
            return Some(outcome);
        }
        self.read_playlist_and_run(record, cancel).await;
        None
    }

    /// Read the download's playlist, keep its qualities, and run the chosen
    /// one when it needs no choice.
    pub(super) async fn read_playlist_and_run(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: watch::Receiver<bool>,
    ) {
        let found = self.look_up_playlist(record, &cancel).await;
        if self.finish_lookup_from(record, found, PlanOrigin::Playlist) {
            self.run_video(record, cancel).await;
        }
    }

    /// The download's playlist as a page and its quality list.
    async fn look_up_playlist(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: &watch::Receiver<bool>,
    ) -> LookupOutcome {
        let (id, link, referer, user_agent) = {
            let rec = record.lock().unwrap();
            let link = rec.url.clone();
            (
                rec.id.clone(),
                link,
                rec.referer.clone(),
                rec.user_agent.clone(),
            )
        };
        let ctx = RequestContext {
            cookies: self.cookies_for(&id),
            referer,
            user_agent,
            title: None,
        };
        read_playlist_page(&link, Some(&ctx), cancel).await
    }
}

/// The playlist at `link` as a page and its quality list, read with what the
/// browser (or the download) sent. Needs no download row.
pub(super) async fn read_playlist_page(
    link: &str,
    ctx: Option<&RequestContext>,
    cancel: &watch::Receiver<bool>,
) -> LookupOutcome {
    let ctx = ctx.filter(|ctx| !ctx.is_empty());
    let read = async {
        let client = stream_client(ctx, &[], Timeouts::default())?;
        let url = Url::parse(link).map_err(|_| unreadable())?;
        let (text, at) = fetch_playlist(&client, &url, cancel).await?;
        page_of_playlist(&text, &at)
    };
    match read.await {
        Ok(page) => match build_choices(&page) {
            Ok(choices) => LookupOutcome::Choices(Box::new(page), choices),
            Err(ChoicesError::Drm) => message(DRM),
            Err(ChoicesError::NoFormats) => message(crate::pieces::UNREADABLE_PLAYLIST),
        },
        Err(Error::Cancelled) => LookupOutcome::Cancelled,
        Err(Error::Video { message, transient }) => LookupOutcome::Message {
            text: message,
            transient,
        },
        Err(e) => LookupOutcome::Message {
            transient: e.is_transient(),
            text: e.to_string(),
        },
    }
}

fn message(text: &str) -> LookupOutcome {
    LookupOutcome::Message {
        text: text.to_string(),
        transient: false,
    }
}

/// The page a playlist read from `at` makes, named after the link.
fn page_of_playlist(text: &str, at: &Url) -> Result<PageInfo, Error> {
    let title = name_from_link(at);
    let video_error = |text: &str| Error::Video {
        message: text.to_string(),
        transient: false,
    };
    match parse_playlist(text, at) {
        Ok(Playlist::Master(master)) => Ok(page_of_master(&master, &title)),
        Ok(Playlist::Media(media)) if media.ended => Ok(page_of_media(at, &media, &title)),
        Ok(Playlist::Media(_)) => Err(video_error(LIVE)),
        Err(HlsError::Protected) => Err(video_error(DRM)),
        Err(_) => Err(unreadable()),
    }
}

/// Whether the download is a direct playlist link rather than a page.
pub(super) fn is_playlist_download(rec: &TaskRecord) -> bool {
    let from_playlist = rec
        .video
        .as_ref()
        .is_some_and(|plan| plan.origin == PlanOrigin::Playlist);
    from_playlist || is_m3u8_link(&rec.url)
}

/// True for a link whose path ends in `.m3u8`: read as a playlist, not by yt-dlp.
pub(super) fn is_m3u8_link(link: &str) -> bool {
    Url::parse(link).is_ok_and(|url| url.path().to_ascii_lowercase().ends_with(".m3u8"))
}

/// A name for the video from its link: the last part without `.m3u8`, else
/// the folder before it when the name says nothing (`master`, `index_3`…),
/// else the host.
pub(super) fn name_from_link(url: &Url) -> String {
    let parts: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|s| !s.is_empty())
                .map(|s| {
                    percent_encoding::percent_decode_str(s)
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    let stem = |part: &str| {
        let lower = part.to_ascii_lowercase();
        match lower.strip_suffix(".m3u8") {
            Some(_) => part[..part.len() - ".m3u8".len()].to_string(),
            None => part.to_string(),
        }
    };
    let found = parts
        .iter()
        .rev()
        .map(|p| stem(p))
        .find(|p| !p.is_empty() && !is_generic(p));
    found.unwrap_or_else(|| {
        let host = url.host_str().unwrap_or("video");
        host.strip_prefix("www.").unwrap_or(host).to_string()
    })
}

/// `master`, `index_3`, `Playlist`, `chunklist5`…
fn is_generic(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let base = lower.trim_end_matches(|c: char| c.is_ascii_digit());
    let base = base.strip_suffix('_').unwrap_or(base);
    GENERIC_NAMES.contains(&base)
}

fn codec_with(codecs: &str, prefixes: &[&str]) -> Option<String> {
    codecs
        .split(',')
        .map(str::trim)
        .find(|c| prefixes.iter().any(|p| c.starts_with(p)))
        .map(String::from)
}

/// The sound track a quality of `group` uses: the default one with a link,
/// else the first with a link; None when the sound is inside the quality.
fn audio_of<'a>(master: &'a Master, group: &str) -> Option<&'a AudioRendition> {
    let in_group = || {
        master
            .audio
            .iter()
            .filter(move |a| a.group == group && a.url.is_some())
    };
    in_group().find(|a| a.default).or_else(|| in_group().next())
}

fn hls_format(id: String, url: &Url) -> FormatInfo {
    FormatInfo {
        format_id: id,
        protocol: Some(HLS.to_string()),
        url: Some(url.to_string()),
        ext: Some("mp4".to_string()),
        ..FormatInfo::default()
    }
}

/// A master playlist as yt-dlp would list it: each quality a video stream
/// (with its sound track a separate audio stream) or a whole stream.
pub(super) fn page_of_master(master: &Master, title: &str) -> PageInfo {
    let mut formats = Vec::new();
    let mut tracks: Vec<FormatInfo> = Vec::new();
    for (i, variant) in master.variants.iter().enumerate() {
        let codecs = variant.codecs.as_deref().unwrap_or_default();
        let (video, sound) = (
            codec_with(codecs, &VIDEO_CODECS),
            codec_with(codecs, &AUDIO_CODECS),
        );
        let id = match variant.height {
            Some(height) => format!("hls-{height}-{i}"),
            None => format!("hls-{i}"),
        };
        let track = variant
            .audio_group
            .as_deref()
            .and_then(|group| audio_of(master, group));
        let mut format = FormatInfo {
            height: variant.height,
            tbr: variant.bandwidth.map(|b| b as f64 / 1000.0),
            ..hls_format(id, &variant.url)
        };
        match track {
            Some(track) => {
                format.vcodec = Some(video.unwrap_or_else(|| "avc1".to_string()));
                format.acodec = Some("none".to_string());
                let track_id = format!("audio-{}", track.group);
                if !tracks.iter().any(|t| t.format_id == track_id) {
                    tracks.push(audio_track(track_id, track, sound));
                }
            }
            None if variant.codecs.is_some() => {
                format.vcodec = video;
                format.acodec = sound;
            }
            None => {}
        }
        formats.push(format);
    }
    formats.extend(tracks);
    PageInfo {
        title: title.to_string(),
        formats,
        ..PageInfo::default()
    }
}

fn audio_track(id: String, track: &AudioRendition, codec: Option<String>) -> FormatInfo {
    let codec = codec.unwrap_or_else(|| "mp4a".to_string());
    let ext = if codec.starts_with("mp4a") {
        "m4a"
    } else {
        "mp4"
    };
    let url = track.url.as_ref().expect("tracks with a link only");
    FormatInfo {
        vcodec: Some("none".to_string()),
        acodec: Some(codec),
        ext: Some(ext.to_string()),
        ..hls_format(id, url)
    }
}

/// A playlist of pieces: one quality, video and sound together.
pub(super) fn page_of_media(url: &Url, media: &Media, title: &str) -> PageInfo {
    let millis: u64 = media.pieces.iter().map(|p| p.duration_ms).sum();
    PageInfo {
        title: title.to_string(),
        duration: Some((millis as f64 / 1000.0).round()),
        formats: vec![hls_format("hls".to_string(), url)],
        ..PageInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::choices::build_choices;
    use crate::video::hls::{parse_playlist, Playlist};
    use crate::video::{Container, StreamKind};

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn master(text: &str) -> Master {
        match parse_playlist(text, &url("https://cdn.test/v/master.m3u8")) {
            Ok(Playlist::Master(master)) => master,
            other => panic!("{other:?}"),
        }
    }

    const APPLE_LIKE: &str = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud1\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"a1/prog_index.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud1\",NAME=\"Other\",LANGUAGE=\"fr\",URI=\"a2/prog_index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=2177116,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=960x540,AUDIO=\"aud1\"
v5/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=8001098,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"aud1\"
v9/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3216424,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"aud1\"
v6/prog_index.m3u8
";

    #[test]
    fn names_come_from_the_link() {
        let name = |link: &str| name_from_link(&url(link));

        assert_eq!(
            name("https://devstreaming-cdn.apple.com/x/img_bipbop_adv_example_fmp4/master.m3u8"),
            "img_bipbop_adv_example_fmp4"
        );
        assert_eq!(name("https://cdn.test/show/ep1/index_3.m3u8"), "ep1");
        assert_eq!(name("https://cdn.test/clip.m3u8?tok=1"), "clip");
        assert_eq!(name("https://cdn.test/a/Playlist.M3U8"), "a");
        assert_eq!(name("https://www.cdn.test/master.m3u8"), "cdn.test");
    }

    #[test]
    fn a_master_becomes_formats_the_rules_accept() {
        let page = page_of_master(&master(APPLE_LIKE), "Clip");

        let choices = build_choices(&page).unwrap();

        assert_eq!(choices.title, "Clip");
        let labels: Vec<&str> = choices.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["1080p", "720p", "540p", "Audio only"]);
        let top = &choices.choices[choices.default];
        assert_eq!(top.label, "1080p");
        assert_eq!(top.container, Container::Mp4);
        let audio_id = &top.streams[1].format_id;
        let audio = page
            .formats
            .iter()
            .find(|f| &f.format_id == audio_id)
            .unwrap();
        assert_eq!(
            audio.url.as_deref(),
            Some("https://cdn.test/v/a1/prog_index.m3u8"),
            "the DEFAULT=YES track"
        );
        assert!(page
            .formats
            .iter()
            .all(|f| f.protocol.as_deref() == Some("m3u8_native")));
    }

    #[test]
    fn a_variant_without_an_audio_group_is_whole() {
        let text = "#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720
720/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=400000,CODECS=\"avc1.4d401e,mp4a.40.2\",RESOLUTION=640x360
360/index.m3u8
";
        let page = page_of_master(&master(text), "Clip");

        let choices = build_choices(&page).unwrap();

        let labels: Vec<&str> = choices.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["720p", "360p"]);
        assert!(choices
            .choices
            .iter()
            .all(|c| c.streams.len() == 1 && c.streams[0].kind == StreamKind::Both));
    }

    #[test]
    fn a_media_playlist_is_one_quality() {
        let link = url("https://cdn.test/v/clip.m3u8");
        let text = "#EXTM3U\n#EXTINF:4.5,\na.ts\n#EXTINF:4.5,\nb.ts\n#EXT-X-ENDLIST\n";
        let Ok(Playlist::Media(media)) = parse_playlist(text, &link) else {
            panic!("not media");
        };

        let page = page_of_media(&link, &media, "clip");

        let choices = build_choices(&page).unwrap();
        assert_eq!(choices.choices.len(), 1);
        assert_eq!(choices.choices[0].streams[0].kind, StreamKind::Both);
        assert_eq!(choices.duration_secs, Some(9));
        assert_eq!(page.formats[0].url.as_deref(), Some(link.as_str()));
    }
}
