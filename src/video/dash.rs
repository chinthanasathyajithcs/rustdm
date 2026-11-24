//! A DASH format's pieces, as yt-dlp lists them (`fragments` beside a
//! `fragment_base_url`); the first is the init piece (spec 1c, section 1.2).

use super::info::FormatInfo;
use super::DashPiece;
use url::Url;

/// Every piece's link and length, in order; None without pieces, or when one
/// cannot be made into a link.
pub fn dash_pieces(format: &FormatInfo) -> Option<Vec<DashPiece>> {
    if format.fragments.is_empty() {
        return None;
    }
    let base = format.fragment_base_url.as_deref().map(Url::parse);
    format
        .fragments
        .iter()
        .map(|fragment| {
            let link = match (&fragment.url, &fragment.path, &base) {
                (Some(url), _, _) => Url::parse(url).ok(),
                (None, Some(path), Some(Ok(base))) => base.join(path).ok(),
                _ => None,
            }?;
            let seconds = fragment.duration.filter(|d| d.is_finite() && *d > 0.0);
            Some(DashPiece {
                link: link.into(),
                duration_ms: seconds.map_or(0, |s| (s * 1000.0).round() as u64),
            })
        })
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
            dash_pieces(&format),
            Some(vec![
                DashPiece {
                    link: "https://d.test/a/v/0.m4s".to_string(),
                    duration_ms: 0,
                },
                DashPiece {
                    link: "https://x.test/1.m4s".to_string(),
                    duration_ms: 4000,
                },
            ])
        );
    }

    #[test]
    fn no_fragments_or_a_bad_link_is_none() {
        assert_eq!(dash_pieces(&FormatInfo::default()), None);
        let relative_without_base = FormatInfo {
            fragments: vec![Fragment {
                path: Some("v/0.m4s".to_string()),
                ..Fragment::default()
            }],
            ..FormatInfo::default()
        };
        assert_eq!(dash_pieces(&relative_without_base), None);
    }
}
