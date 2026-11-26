//! What to tell the user when a video lookup does not give a video: yt-dlp's
//! trouble and the pages that have nothing rustdm can download.

use super::choices::ChoicesError;
use super::info::PageInfo;

/// Why running yt-dlp did not give a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupFailure {
    TimedOut,
    TooMuchOutput,
    /// yt-dlp exited 0 but its output was not the JSON rustdm reads.
    NotJson(String),
    Exited {
        code: Option<i32>,
        stderr: String,
    },
    /// yt-dlp could not be started, or the lookup was cancelled (`"cancelled"`).
    Spawn(String),
}

/// What a page with nothing to download is told.
pub const NO_VIDEO: &str = "This link is a web page, not a file, and no video was found on it.";
/// The message of a join that failed; `folder` holds the two kept parts.
pub fn join_failed_message(last_line: &str, folder: &std::path::Path) -> String {
    format!(
        "Couldn't join the video and audio: {last_line}. The two parts are kept in {}; \
         press r to try again.",
        folder.display()
    )
}
const SIGN_IN_NEEDED: &str =
    "This video needs you to be signed in. Add it from your browser so rustdm gets your sign-in.";
pub const LIVE: &str = "This is a live stream; rustdm downloads finished videos only.";
pub const DRM: &str = "This video is copy-protected; rustdm can't download it.";
const TIMED_OUT: &str = "yt-dlp took more than a minute to read this page; press r to try again.";
const TOO_MUCH_OUTPUT: &str = "yt-dlp gave more output than expected";
/// The longest yt-dlp error line shown as it is.
const MAX_ERROR_CHARS: usize = 200;

/// yt-dlp's words (lower case) for a video that wants a signed-in viewer.
const SIGN_IN_MARKERS: [&str; 6] = [
    "sign in to confirm",
    "age-restricted",
    "members-only",
    "private video",
    "this video is private",
    "login required",
];
const COUNTRY_MARKERS: [&str; 2] = ["available in your country", "geo restrict"];
const NETWORK_MARKERS: [&str; 5] = [
    "unable to download",
    "timed out",
    "connection reset",
    "temporary failure in name resolution",
    "network is unreachable",
];

/// A stderr line that reports trouble reaching the site. `Unable to download`
/// also covers pages that are gone (`HTTP Error 404`), which will not come
/// back, so it counts only when its line has no 4xx status but 429.
fn is_network_line(line: &str) -> bool {
    let line = line.to_lowercase();
    if line.contains(NETWORK_MARKERS[0]) && has_permanent_http_error(&line) {
        return NETWORK_MARKERS[1..]
            .iter()
            .any(|marker| line.contains(marker));
    }
    NETWORK_MARKERS.iter().any(|marker| line.contains(marker))
}

/// Whether the (lower case) line says `http error 4xx` for a 4xx other than 429.
fn has_permanent_http_error(line: &str) -> bool {
    const PREFIX: &str = "http error ";
    line.match_indices(PREFIX).any(|(at, _)| {
        let status = &line[at + PREFIX.len()..];
        status.starts_with('4') && !status.starts_with("429")
    })
}

fn mentions(text: &str, markers: &[&str]) -> bool {
    let text = text.to_lowercase();
    markers.iter().any(|marker| text.contains(marker))
}

/// The message for a failed lookup, and whether trying again later may help.
pub fn failure_message(
    failure: &LookupFailure,
    site: &str,
    has_cookies: bool,
    ytdlp_version: &str,
) -> (String, bool) {
    match failure {
        LookupFailure::TimedOut => (TIMED_OUT.to_string(), true),
        LookupFailure::TooMuchOutput => (TOO_MUCH_OUTPUT.to_string(), false),
        LookupFailure::NotJson(detail) => (
            format!(
                "yt-dlp gave an answer rustdm couldn't read: {}",
                clip(detail)
            ),
            false,
        ),
        LookupFailure::Spawn(detail) => (format!("Couldn't start yt-dlp: {detail}"), true),
        LookupFailure::Exited { code, stderr } => {
            let text = exited_message(*code, stderr, site, has_cookies, ytdlp_version);
            (text, stderr.lines().any(is_network_line))
        }
    }
}

fn exited_message(
    code: Option<i32>,
    stderr: &str,
    site: &str,
    has_cookies: bool,
    ytdlp_version: &str,
) -> String {
    if mentions(stderr, &SIGN_IN_MARKERS) {
        return if has_cookies {
            format!(
                "{site} didn't accept your sign-in for this video. \
                 Sign in again in the browser and add it from there."
            )
        } else {
            SIGN_IN_NEEDED.to_string()
        };
    }
    if mentions(stderr, &COUNTRY_MARKERS) {
        return format!("{site} says this video isn't available in your country.");
    }
    if stderr.contains("Unsupported URL") {
        return NO_VIDEO.to_string();
    }
    if is_extractor_error(stderr) {
        return format!(
            "yt-dlp {ytdlp_version} couldn't read this page; the site may have changed. \
             rustdm will try again with the next yt-dlp."
        );
    }
    match error_line(stderr) {
        Some(line) => clip(line),
        None => match code {
            Some(code) => format!("yt-dlp stopped with exit code {code}"),
            None => "yt-dlp stopped without finishing".to_string(),
        },
    }
}

/// The last `ERROR:` line of yt-dlp's stderr, without the `ERROR: ` prefix.
fn error_line(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("ERROR:"))
        .map(str::trim)
}

fn clip(text: &str) -> String {
    text.trim().chars().take(MAX_ERROR_CHARS).collect()
}

/// True when yt-dlp's own extractor for the site broke (an `ERROR: [site]` line
/// that is not a sign-in, country or network problem): a newer yt-dlp may fix it.
pub fn is_extractor_error(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        line.trim().starts_with("ERROR: [")
            && !mentions(line, &SIGN_IN_MARKERS)
            && !mentions(line, &COUNTRY_MARKERS)
            && !is_network_line(line)
            && !line.to_lowercase().contains(NETWORK_MARKERS[0])
    })
}

/// The message for a page that was read but has nothing to download, or `None`
/// when it has qualities to choose from.
pub fn page_message(info: &PageInfo, err: Option<ChoicesError>, _site: &str) -> Option<String> {
    if info.kind.as_deref() == Some("playlist") {
        let count = info
            .playlist_count
            .map(|n| n as usize)
            .or(info.entries.as_ref().map(Vec::len));
        return Some(match count {
            Some(n) => {
                format!("This is a playlist of {n} videos; playlists come in a later version.")
            }
            None => "This is a playlist; playlists come in a later version.".to_string(),
        });
    }
    if is_live(info) {
        return Some(LIVE.to_string());
    }
    let text = match err? {
        ChoicesError::Drm => DRM,
        ChoicesError::NoFormats => NO_VIDEO,
    };
    Some(text.to_string())
}

fn is_live(info: &PageInfo) -> bool {
    info.is_live == Some(true)
        || matches!(
            info.live_status.as_deref(),
            Some("is_live") | Some("is_upcoming")
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(stderr: &str) -> LookupFailure {
        LookupFailure::Exited {
            code: Some(1),
            stderr: stderr.to_string(),
        }
    }

    fn message(failure: &LookupFailure, has_cookies: bool) -> String {
        failure_message(failure, "YouTube", has_cookies, "2026.11.10").0
    }

    #[test]
    fn a_sign_in_wall_asks_to_add_the_video_from_the_browser() {
        let failure = exited("ERROR: [youtube] abc: Sign in to confirm you're not a bot");

        let (text, transient) = failure_message(&failure, "YouTube", false, "2026.11.10");

        assert_eq!(
            text,
            "This video needs you to be signed in. Add it from your browser so rustdm gets your sign-in."
        );
        assert!(!transient);
    }

    #[test]
    fn a_sign_in_wall_with_cookies_says_the_site_refused_them() {
        let failure =
            exited("ERROR: [youtube] abc: Private video. Sign in if you've been granted access");

        assert_eq!(
            message(&failure, true),
            "YouTube didn't accept your sign-in for this video. Sign in again in the browser and add it from there."
        );
    }

    #[test]
    fn every_sign_in_or_age_marker_counts() {
        for marker in [
            "Sign in to confirm your age",
            "This video may be inappropriate for some users: age-restricted",
            "Join this channel to get access: members-only content",
            "Private video",
            "This video is private",
            "login required",
        ] {
            let text = message(&exited(&format!("ERROR: [site] x: {marker}")), false);
            assert!(
                text.starts_with("This video needs you to be signed in"),
                "{marker}"
            );
        }
    }

    #[test]
    fn a_country_block_names_the_site() {
        let failure = exited(
            "ERROR: [youtube] abc: The uploader has not made this video available in your country",
        );

        assert_eq!(
            message(&failure, false),
            "YouTube says this video isn't available in your country."
        );
        assert_eq!(
            message(&exited("ERROR: [x] 1: geo restricted"), false),
            "YouTube says this video isn't available in your country."
        );
    }

    #[test]
    fn an_unsupported_link_is_a_page_without_a_video() {
        let failure = exited("ERROR: Unsupported URL: https://example.com/page");

        assert_eq!(
            message(&failure, false),
            "This link is a web page, not a file, and no video was found on it."
        );
    }

    #[test]
    fn a_broken_extractor_names_the_yt_dlp_version() {
        let failure = exited("ERROR: [youtube] abc: Unable to extract initial player response");

        assert_eq!(
            message(&failure, false),
            "yt-dlp 2026.11.10 couldn't read this page; the site may have changed. rustdm will try again with the next yt-dlp."
        );
    }

    #[test]
    fn a_timeout_says_so_and_may_be_retried() {
        let (text, transient) = failure_message(&LookupFailure::TimedOut, "Site", false, "1");

        assert_eq!(
            text,
            "yt-dlp took more than a minute to read this page; press r to try again."
        );
        assert!(transient);
    }

    #[test]
    fn too_much_output_is_refused_for_good() {
        let (text, transient) = failure_message(&LookupFailure::TooMuchOutput, "Site", false, "1");

        assert_eq!(text, "yt-dlp gave more output than expected");
        assert!(!transient);
    }

    #[test]
    fn a_program_that_will_not_start_may_work_later() {
        let failure = LookupFailure::Spawn("No such file".to_string());

        let (text, transient) = failure_message(&failure, "Site", false, "1");

        assert_eq!(text, "Couldn't start yt-dlp: No such file");
        assert!(transient);
    }

    #[test]
    fn an_answer_that_is_not_json_is_not_retried() {
        let failure = LookupFailure::NotJson("expected value at line 1".to_string());

        let (text, transient) = failure_message(&failure, "Site", false, "1");

        assert_eq!(
            text,
            "yt-dlp gave an answer rustdm couldn't read: expected value at line 1"
        );
        assert!(!transient);
    }

    #[test]
    fn anything_else_shows_the_error_line_without_its_prefix() {
        let failure = exited("WARNING: x\nERROR: HTTP Error 500: Internal Server Error\n");

        assert_eq!(
            message(&failure, false),
            "HTTP Error 500: Internal Server Error"
        );
    }

    #[test]
    fn the_error_line_is_trimmed_to_200_characters() {
        let failure = exited(&format!("ERROR: {}", "é".repeat(300)));

        let text = message(&failure, false);

        assert_eq!(text.chars().count(), 200);
        assert!(text.starts_with("éé"));
    }

    #[test]
    fn with_no_error_line_the_exit_code_is_reported() {
        let failure = LookupFailure::Exited {
            code: Some(2),
            stderr: String::new(),
        };

        assert_eq!(message(&failure, false), "yt-dlp stopped with exit code 2");
    }

    #[test]
    fn network_errors_are_transient_and_others_are_not() {
        for marker in [
            "Unable to download webpage: <urlopen error>",
            "The read operation timed out",
            "Connection reset by peer",
            "Temporary failure in name resolution",
            "Network is unreachable",
        ] {
            let failure = exited(&format!("ERROR: [youtube] abc: {marker}"));
            let (_, transient) = failure_message(&failure, "YouTube", false, "1");
            assert!(transient, "{marker}");
            assert!(!is_extractor_error(&format!(
                "ERROR: [youtube] abc: {marker}"
            )));
        }
        let (_, transient) = failure_message(
            &exited("ERROR: [youtube] abc: Private video"),
            "Y",
            false,
            "1",
        );
        assert!(!transient);
    }

    #[test]
    fn a_page_that_is_gone_is_not_retried_but_a_busy_site_is() {
        let transient = |line: &str| failure_message(&exited(line), "Y", false, "1").1;

        for status in [
            "400: Bad Request",
            "403: Forbidden",
            "404: Not Found",
            "410: Gone",
        ] {
            let line =
                format!("ERROR: [generic] x: Unable to download webpage: HTTP Error {status}");
            assert!(!transient(&line), "{status}");
        }
        assert!(transient(
            "ERROR: [generic] x: Unable to download webpage: HTTP Error 429: Too Many Requests"
        ));
        assert!(transient(
            "ERROR: [generic] x: Unable to download webpage: HTTP Error 503: Service Unavailable"
        ));
        assert!(transient(
            "ERROR: [generic] x: Unable to download webpage: <urlopen error timed out>"
        ));
        // A 404 on one line does not make another line's timeout permanent.
        assert!(transient(
            "ERROR: [generic] x: Unable to download webpage: HTTP Error 404: Not Found\nERROR: [generic] y: The read operation timed out"
        ));
    }

    #[test]
    fn a_page_that_is_gone_shows_its_own_line_not_a_broken_extractor() {
        let failure =
            exited("ERROR: [generic] x: Unable to download webpage: HTTP Error 404: Not Found");

        assert!(!is_extractor_error(
            "ERROR: [generic] x: Unable to download webpage: HTTP Error 404: Not Found"
        ));
        assert_eq!(
            message(&failure, false),
            "[generic] x: Unable to download webpage: HTTP Error 404: Not Found"
        );
    }

    #[test]
    fn only_a_broken_extractor_is_an_extractor_error() {
        assert!(is_extractor_error(
            "ERROR: [youtube] abc: Unable to extract initial player response"
        ));
        assert!(!is_extractor_error(
            "ERROR: [youtube] abc: Sign in to confirm you're not a bot"
        ));
        assert!(!is_extractor_error(
            "ERROR: [youtube] abc: The uploader has not made this video available in your country"
        ));
        assert!(!is_extractor_error(
            "ERROR: Unsupported URL: https://x.test/"
        ));
        assert!(!is_extractor_error("WARNING: [youtube] something"));
        assert!(!is_extractor_error(""));
    }

    fn page(is_live: Option<bool>, live_status: Option<&str>, kind: Option<&str>) -> PageInfo {
        PageInfo {
            is_live,
            live_status: live_status.map(String::from),
            kind: kind.map(String::from),
            ..PageInfo::default()
        }
    }

    #[test]
    fn a_live_stream_is_refused_by_either_flag() {
        let live = "This is a live stream; rustdm downloads finished videos only.";

        assert_eq!(
            page_message(&page(Some(true), None, None), None, "S").as_deref(),
            Some(live)
        );
        assert_eq!(
            page_message(&page(None, Some("is_upcoming"), None), None, "S").as_deref(),
            Some(live)
        );
        assert_eq!(
            page_message(&page(Some(false), Some("is_live"), None), None, "S").as_deref(),
            Some(live)
        );
        assert_eq!(
            page_message(&page(Some(false), Some("not_live"), None), None, "S"),
            None
        );
    }

    #[test]
    fn a_playlist_says_how_many_videos() {
        let mut info = page(None, None, Some("playlist"));
        info.playlist_count = Some(24);

        assert_eq!(
            page_message(&info, Some(ChoicesError::NoFormats), "S").as_deref(),
            Some("This is a playlist of 24 videos; playlists come in a later version.")
        );
    }

    #[test]
    fn a_playlist_of_unknown_length_still_says_it_is_one() {
        let info = page(None, None, Some("playlist"));

        assert_eq!(
            page_message(&info, None, "S").as_deref(),
            Some("This is a playlist; playlists come in a later version.")
        );
    }

    #[test]
    fn pages_with_nothing_to_download_each_get_their_message() {
        let info = page(None, None, Some("video"));

        let text = |err| page_message(&info, Some(err), "S");

        assert_eq!(
            text(ChoicesError::Drm).as_deref(),
            Some("This video is copy-protected; rustdm can't download it.")
        );
        assert_eq!(
            text(ChoicesError::NoFormats).as_deref(),
            Some("This link is a web page, not a file, and no video was found on it.")
        );
    }

    #[test]
    fn a_page_with_qualities_has_no_message() {
        assert_eq!(
            page_message(&page(None, None, Some("video")), None, "S"),
            None
        );
    }
}
