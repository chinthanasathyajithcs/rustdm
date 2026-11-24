//! The request headers a stream keeps from yt-dlp's list. yt-dlp's headers
//! can carry cookies and credentials, and the plan is saved to disk, so only
//! a short list of harmless names passes.

use reqwest::header::HeaderValue;
use std::collections::BTreeMap;

/// The names that pass, compared without regard to letter case.
const ALLOWED: [&str; 6] = [
    "user-agent",
    "accept",
    "accept-language",
    "sec-fetch-mode",
    "referer",
    "origin",
];

/// The longest value that passes.
const MAX_VALUE_BYTES: usize = 1024;

fn is_allowed_name(name: &str) -> bool {
    ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(name))
}

fn is_usable_value(value: &str) -> bool {
    value.len() <= MAX_VALUE_BYTES && HeaderValue::from_str(value).is_ok()
}

/// Keep the allowed headers in their original spelling, sorted by name.
/// Dropped names are logged; their values never are.
pub fn allowed_headers(raw: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut kept = Vec::new();
    for (name, value) in raw {
        if is_allowed_name(name) && is_usable_value(value) {
            kept.push((name.clone(), value.clone()));
        } else {
            tracing::debug!("dropped the stream header {name}");
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_the_six_allowed_headers_pass_in_any_letter_case() {
        let headers = raw(&[
            ("user-agent", "ua"),
            ("ACCEPT", "*/*"),
            ("Accept-Language", "en"),
            ("Sec-Fetch-Mode", "navigate"),
            ("Referer", "https://a.test/"),
            ("Origin", "https://a.test"),
            ("Cookie", "sid=abc"),
            ("Authorization", "Bearer x"),
            ("Host", "a.test"),
            ("Range", "bytes=0-"),
            ("Proxy-Authorization", "Basic x"),
            ("X-Foo", "bar"),
        ]);

        let kept = allowed_headers(&headers);

        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "ACCEPT",
                "Accept-Language",
                "Origin",
                "Referer",
                "Sec-Fetch-Mode",
                "user-agent"
            ]
        );
        assert_eq!(kept[0].1, "*/*");
    }

    #[test]
    fn a_value_over_1024_bytes_or_with_a_newline_is_dropped() {
        let headers = raw(&[
            ("Accept", &"a".repeat(1025)),
            ("Origin", "https://a.test\r\nX-Evil: 1"),
            ("Referer", "https://a.test/\nmore"),
            ("User-Agent", &"u".repeat(1024)),
        ]);

        let kept = allowed_headers(&headers);

        assert_eq!(kept, [("User-Agent".to_string(), "u".repeat(1024))]);
    }
}
