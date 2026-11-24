//! A DASH format's pieces, as yt-dlp lists them (`fragments` beside a
//! `fragment_base_url`); the first is the init piece (spec 1c, section 1.2).

use super::info::FormatInfo;
use url::Url;

/// Every piece's link, in order; None without pieces, or when one cannot be
/// made into a link.
pub fn dash_links(format: &FormatInfo) -> Option<Vec<String>> {
    if format.fragments.is_empty() {
        return None;
    }
    let base = format.fragment_base_url.as_deref().map(Url::parse);
    format
        .fragments
        .iter()
        .map(|fragment| match (&fragment.url, &fragment.path, &base) {
            (Some(url), _, _) => Url::parse(url).ok(),
            (None, Some(path), Some(Ok(base))) => base.join(path).ok(),
            _ => None,
        })
        .map(|link| link.map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::info::Fragment;

    #[test]
    fn paths_join_the_base_and_urls_stay() {
        let format = FormatInfo {
            fragment_base_url: Some("https://d.test/a/./".to_string()),
            fragments: vec![
                Fragment {
                    path: Some("v/0.m4s".to_string()),
                    ..Fragment::default()
                },
                Fragment {
                    url: Some("https://x.test/1.m4s".to_string()),
                    duration: Some(4.0),
                    ..Fragment::default()
                },
            ],
            ..FormatInfo::default()
        };

        assert_eq!(
            dash_links(&format),
            Some(vec![
                "https://d.test/a/v/0.m4s".to_string(),
                "https://x.test/1.m4s".to_string(),
            ])
        );
    }

    #[test]
    fn no_fragments_or_a_bad_link_is_none() {
        assert_eq!(dash_links(&FormatInfo::default()), None);
        let relative_without_base = FormatInfo {
            fragments: vec![Fragment {
                path: Some("v/0.m4s".to_string()),
                ..Fragment::default()
            }],
            ..FormatInfo::default()
        };
        assert_eq!(dash_links(&relative_without_base), None);
    }
}
