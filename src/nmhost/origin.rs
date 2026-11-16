//! Who may start the host. Chrome passes the calling extension's origin as the
//! first argument; the host runs only for the extensions listed here.

/// Extension IDs allowed to use the host. The first is the development ID that
/// the `key` in `extension/manifest.json` produces; store IDs are added when
/// the extension is published (the store package is built without `key`).
pub const ALLOWED_EXTENSION_IDS: &[&str] = &["hmigjmjgnoafbhaojjodabckpddomkdi"];

const ORIGIN_PREFIX: &str = "chrome-extension://";
const EXTENSION_ID_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OriginError {
    #[error("not started by a browser: no caller origin was given")]
    Missing,
    #[error("caller is not a known rustdm extension")]
    Unknown,
}

/// Check the host's command-line arguments (without the program name). Only
/// the first one counts; Chrome's later `--parent-window=` is ignored.
/// Returns the allowed extension ID.
pub fn check_caller(args: &[String]) -> Result<&'static str, OriginError> {
    let origin = args.first().ok_or(OriginError::Missing)?;
    let id = origin
        .strip_prefix(ORIGIN_PREFIX)
        .and_then(|rest| rest.strip_suffix('/'))
        .filter(|id| id.len() == EXTENSION_ID_LEN && id.bytes().all(|b| (b'a'..=b'p').contains(&b)))
        .ok_or(OriginError::Unknown)?;
    ALLOWED_EXTENSION_IDS
        .iter()
        .find(|allowed| **allowed == id)
        .copied()
        .ok_or(OriginError::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn origin_of(id: &str) -> String {
        format!("chrome-extension://{id}/")
    }

    #[test]
    fn the_listed_ids_are_well_formed() {
        for id in ALLOWED_EXTENSION_IDS {
            assert_eq!(id.len(), EXTENSION_ID_LEN);
            assert!(id.bytes().all(|b| (b'a'..=b'p').contains(&b)), "{id}");
        }
    }

    #[test]
    fn an_allowed_origin_passes() {
        let id = ALLOWED_EXTENSION_IDS[0];
        assert_eq!(check_caller(&[origin_of(id)]), Ok(id));
    }

    #[test]
    fn the_parent_window_argument_is_ignored() {
        let id = ALLOWED_EXTENSION_IDS[0];
        let with_window = [origin_of(id), "--parent-window=1234".to_string()];
        assert_eq!(check_caller(&with_window), Ok(id));
        // It cannot stand in for the origin.
        assert_eq!(
            check_caller(&args(&["--parent-window=1234"])),
            Err(OriginError::Unknown)
        );
    }

    #[test]
    fn another_extension_is_refused() {
        let other = "a".repeat(32);
        assert_eq!(
            check_caller(&[origin_of(&other)]),
            Err(OriginError::Unknown)
        );
    }

    #[test]
    fn a_missing_origin_is_refused() {
        assert_eq!(check_caller(&[]), Err(OriginError::Missing));
    }

    #[test]
    fn near_misses_are_refused() {
        let id = ALLOWED_EXTENSION_IDS[0];
        for bad in [
            id.to_string(),
            format!("chrome-extension://{id}"),
            format!("chrome-extension://{id}/extra"),
            format!("https://{id}/"),
            format!("chrome-extension://{}/", id.to_uppercase()),
            format!("chrome-extension://{}/", &id[1..]),
            format!("chrome-extension://{id}x/"),
            String::new(),
        ] {
            assert_eq!(
                check_caller(&[bad.clone()]),
                Err(OriginError::Unknown),
                "{bad}"
            );
        }
    }
}
