//! Expired stream links get fresh ones from yt-dlp (spec 6.4): how often that
//! may happen, and whether the fresh streams are still the same video.

use super::StreamPlan;

/// At most this many refreshes ...
pub const REFRESH_LIMIT: usize = 3;
/// ... within this many seconds.
pub const REFRESH_WINDOW_SECS: u64 = 600;

/// What a row says while a fresh link is being fetched.
pub const REFRESH_STEP: &str = "Link expired, getting a fresh one…";

/// A refreshed video whose streams are not the ones being downloaded.
pub const VIDEO_CHANGED: &str =
    "This video changed on the site; download it again to get the new version.";

/// Why a direct playlist link paused: a fresh read of its playlist did not help.
pub const DIRECT_EXPIRED: &str = "This video's link expired; add it again from its page.";

/// Why a video paused after too many refreshes.
pub fn refresh_limit_message(site: &str) -> String {
    format!("{site} keeps refusing this video's links; press r to try again later.")
}

/// The fresh page no longer has the streams being downloaded.
#[derive(Debug, PartialEq, Eq)]
pub struct Changed;

fn in_window(time: u64, now_unix: u64) -> bool {
    now_unix.saturating_sub(time) < REFRESH_WINDOW_SECS
}

/// False when `REFRESH_LIMIT` refreshes already lie within the window.
pub fn may_refresh(refreshes: &[u64], now_unix: u64) -> bool {
    let recent = refreshes.iter().filter(|&&t| in_window(t, now_unix));
    recent.count() < REFRESH_LIMIT
}

/// A direct playlist link is read again once per window (spec 1c, section 8).
pub fn may_refresh_playlist(refreshes: &[u64], now_unix: u64) -> bool {
    !refreshes.iter().any(|&t| in_window(t, now_unix))
}

/// Count a refresh at `now_unix`, forgetting the ones outside the window.
pub fn note_refresh(refreshes: &mut Vec<u64>, now_unix: u64) {
    refreshes.retain(|&t| in_window(t, now_unix));
    refreshes.push(now_unix);
}

/// The old streams with the fresh addresses: each is found in `fresh` by its
/// format id, keeps its file, kind and ending, and takes the new URL, headers
/// and request size. A stream that is gone, or whose size differs where both
/// are known, means the video changed.
pub fn refreshed_streams(
    old: &[StreamPlan],
    fresh: &[StreamPlan],
) -> Result<Vec<StreamPlan>, Changed> {
    old.iter()
        .map(|stream| {
            let new = fresh
                .iter()
                .find(|f| f.format_id == stream.format_id)
                .ok_or(Changed)?;
            // A pieces stream's size is an estimate; its pieces are compared
            // when it runs again.
            let both_direct = !stream.source.is_pieces() && !new.source.is_pieces();
            if let (true, Some(a), Some(b)) = (both_direct, stream.size, new.size) {
                if a != b {
                    return Err(Changed);
                }
            }
            Ok(StreamPlan {
                url: new.url.clone(),
                headers: new.headers.clone(),
                request_bytes: new.request_bytes,
                source: new.source.clone(),
                ..stream.clone()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{StreamKind, StreamSource};
    use std::path::PathBuf;

    const T: u64 = 1_700_000_000;

    fn stream(id: &str, size: Option<u64>, url: &str) -> StreamPlan {
        StreamPlan {
            format_id: id.to_string(),
            kind: StreamKind::Video,
            ext: "mp4".to_string(),
            url: url.to_string(),
            size,
            headers: vec![("User-Agent".to_string(), url.to_string())],
            request_bytes: Some(10),
            file: None,
            source: StreamSource::Direct,
        }
    }

    #[test]
    fn pieces_streams_take_their_fresh_source_without_a_size_check() {
        let hls = |size, playlist: &str| StreamPlan {
            source: StreamSource::Hls {
                playlist: playlist.to_string(),
            },
            ..stream("hls-1", Some(size), playlist)
        };

        let streams = refreshed_streams(&[hls(100, "old")], &[hls(130, "new")]).unwrap();

        assert_eq!(
            streams[0].source,
            StreamSource::Hls {
                playlist: "new".to_string()
            }
        );
        assert_eq!(streams[0].size, Some(100));
    }

    #[test]
    fn three_refreshes_in_ten_minutes_are_the_limit() {
        let refreshes = [T, T + 10, T + 20];

        assert!(!may_refresh(&refreshes, T + 30));
        assert!(may_refresh(&refreshes, T + 601));
        assert!(may_refresh(&refreshes[..2], T + 30));
    }

    #[test]
    fn a_playlist_link_is_fetched_again_once_a_window() {
        assert!(may_refresh_playlist(&[], T));
        assert!(!may_refresh_playlist(&[T], T + 30));
        assert!(may_refresh_playlist(&[T], T + 601));
    }

    #[test]
    fn note_refresh_forgets_old_times() {
        let mut refreshes = vec![T, T + 500];

        note_refresh(&mut refreshes, T + 700);

        assert_eq!(refreshes, [T + 500, T + 700]);
    }

    #[test]
    fn a_stream_is_the_same_by_id_and_size() {
        let mut old = stream("137", Some(100), "http://old/137");
        old.file = Some(PathBuf::from("/d/A.f137.mp4"));
        old.kind = StreamKind::Video;
        let mut new = stream("137", Some(100), "http://new/137");
        new.request_bytes = Some(20);
        new.kind = StreamKind::Both;
        new.ext = "mkv".to_string();
        let other = stream("140", Some(5), "http://new/140");

        let streams = refreshed_streams(&[old.clone()], &[other, new.clone()]).unwrap();

        let expected = StreamPlan {
            url: new.url.clone(),
            headers: new.headers.clone(),
            request_bytes: Some(20),
            ..old
        };
        assert_eq!(streams, [expected]);
    }

    #[test]
    fn a_missing_id_or_another_size_is_a_changed_video() {
        let old = [stream("137", Some(100), "a"), stream("140", Some(5), "b")];

        let missing = refreshed_streams(&old, &[stream("137", Some(100), "c")]);
        let resized = [stream("137", Some(101), "c"), stream("140", Some(5), "d")];

        assert_eq!(missing, Err(Changed));
        assert_eq!(refreshed_streams(&old, &resized), Err(Changed));
    }

    #[test]
    fn an_unknown_size_on_either_side_still_matches_by_id() {
        let old = [stream("137", None, "a"), stream("140", Some(5), "b")];
        let fresh = [stream("137", Some(100), "c"), stream("140", None, "d")];

        let streams = refreshed_streams(&old, &fresh).unwrap();

        assert_eq!(streams[0].url, "c");
        assert_eq!(streams[0].size, None);
        assert_eq!(streams[1].url, "d");
        assert_eq!(streams[1].size, Some(5));
    }

    #[test]
    fn the_limit_message_names_the_site() {
        assert_eq!(
            refresh_limit_message("YouTube"),
            "YouTube keeps refusing this video's links; press r to try again later."
        );
    }
}
