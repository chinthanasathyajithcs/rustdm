//! Versions are dotted numbers (`2026.11.14.232819`, `8.1.3`). Comparing them
//! numerically keeps `2026.9` below `2026.10`, which a text compare gets wrong.

use std::cmp::Ordering;

/// `None` unless every dot-separated part is a plain number.
pub fn parse(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        })
        .collect()
}

/// Missing trailing parts count as zero, so `1.2` equals `1.2.0`.
pub fn compare(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    (0..len)
        .map(|i| {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            x.cmp(&y)
        })
        .find(|ordering| ordering.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Whether `candidate` is a strictly newer version than `installed`. A version
/// that is not dotted numbers is never newer, so an odd tag cannot cause a
/// downgrade; an unreadable installed version is always replaced.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    match (parse(candidate), parse(installed)) {
        (Some(c), Some(i)) => compare(&c, &i) == Ordering::Greater,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_not_text() {
        assert!(is_newer("2026.10.1", "2026.9.30"));
        assert!(is_newer("2026.11.14.232819", "2026.11.14"));
        assert!(!is_newer("2026.11.14", "2026.11.14.0"));
        assert!(!is_newer("2026.11.10", "2026.11.14"));
        assert!(!is_newer("2026.11.14", "2026.11.14"));
    }

    #[test]
    fn odd_versions_are_never_newer_and_bad_installs_are_replaced() {
        assert!(!is_newer("nightly", "2026.11.14"));
        assert!(!is_newer("2026.11.x", "2026.11.14"));
        assert!(!is_newer("", "1"));
        assert!(is_newer("1", "garbage"));
        assert_eq!(parse("1..2"), None);
        assert_eq!(parse("+1.2"), None);
    }
}
