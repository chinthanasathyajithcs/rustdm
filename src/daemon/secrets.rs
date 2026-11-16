//! A download's browser cookies, kept on disk only while the download needs
//! them so a restart can carry on. One sealed file per download:
//!
//! ```text
//! b"RDMC" | version (1) | scheme (see seal::SCHEME) | sealed JSON of the cookies
//! ```
//!
//! The sealing entropy names the download, so a file copied onto another
//! download's name does not open.

use crate::daemon::seal;
use crate::request_context::ContextCookie;
use crate::state::write_atomic_private;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// Folder under the data folder that holds the secret files.
pub const SECRETS_DIR: &str = "secrets";
const FILE_EXTENSION: &str = "cookies";
const MAGIC: &[u8; 4] = b"RDMC";
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = MAGIC.len() + 2;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("no saved sign-in")]
    Missing,
    #[error("saved sign-in could not be read: {0}")]
    Unreadable(io::Error),
    #[error("saved sign-in could not be decrypted")]
    Undecryptable,
    #[error("saved sign-in is damaged")]
    Corrupt,
}

impl SecretError {
    /// A short fixed word for logs; never anything from the file.
    pub fn kind(&self) -> &'static str {
        match self {
            SecretError::Missing => "missing",
            SecretError::Unreadable(_) => "unreadable",
            SecretError::Undecryptable => "undecryptable",
            SecretError::Corrupt => "corrupt",
        }
    }
}

pub struct SecretStore {
    dir: PathBuf,
}

/// Download ids look like `dl_12`. Checked before an id becomes part of a path.
pub fn is_download_id(id: &str) -> bool {
    id.strip_prefix("dl_")
        .is_some_and(|n| !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit()))
}

fn entropy_for(id: &str) -> Vec<u8> {
    format!("rustdm-cookies:{id}").into_bytes()
}

impl SecretStore {
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join(SECRETS_DIR),
        }
    }

    fn file_for(&self, id: &str) -> Option<PathBuf> {
        is_download_id(id).then(|| self.dir.join(format!("{id}.{FILE_EXTENSION}")))
    }

    /// Seal `cookies` and save them as download `id`'s secret, replacing any earlier one.
    pub fn save(&self, id: &str, cookies: &[ContextCookie]) -> io::Result<()> {
        let path = self
            .file_for(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a download id"))?;
        self.make_dir()?;
        let json = serde_json::to_vec(cookies).map_err(io::Error::other)?;
        let sealed = seal::seal(&json, &entropy_for(id))?;
        let mut file = Vec::with_capacity(HEADER_LEN + sealed.len());
        file.extend_from_slice(MAGIC);
        file.push(FORMAT_VERSION);
        file.push(seal::SCHEME);
        file.extend_from_slice(&sealed);
        write_atomic_private(&path, &file)
    }

    pub fn load(&self, id: &str) -> Result<Vec<ContextCookie>, SecretError> {
        let path = self.file_for(id).ok_or(SecretError::Missing)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(SecretError::Missing),
            Err(e) => return Err(SecretError::Unreadable(e)),
        };
        let [m0, m1, m2, m3, version, scheme, sealed @ ..] = bytes.as_slice() else {
            return Err(SecretError::Corrupt);
        };
        if [*m0, *m1, *m2, *m3] != *MAGIC || *version != FORMAT_VERSION {
            return Err(SecretError::Corrupt);
        }
        match *scheme {
            s if s == seal::SCHEME => {}
            1 | 2 => return Err(SecretError::Undecryptable),
            _ => return Err(SecretError::Corrupt),
        }
        let json =
            seal::unseal(sealed, &entropy_for(id)).map_err(|_| SecretError::Undecryptable)?;
        serde_json::from_slice(&json).map_err(|_| SecretError::Corrupt)
    }

    /// Remove download `id`'s secret. A secret that is already gone is fine; any
    /// other failure is logged (by kind only), since the caller cannot do more.
    pub fn delete(&self, id: &str) {
        let Some(path) = self.file_for(id) else {
            return;
        };
        for target in [path.clone(), temp_of(&path)] {
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("could not delete a saved sign-in for {id}: {}", e.kind()),
            }
        }
    }

    /// Remove temp files and every secret whose download is not in `keep`.
    pub fn sweep(&self, keep: &HashSet<String>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let orphan = if name.ends_with(".tmp") {
                true
            } else if let Some(id) = name.strip_suffix(&format!(".{FILE_EXTENSION}")) {
                !keep.contains(id)
            } else {
                false
            };
            if orphan {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!("could not remove a leftover sign-in file: {}", e.kind());
                }
            }
        }
    }

    /// Create the secrets folder, closed to other users on Unix.
    fn make_dir(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn temp_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.tmp", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str) -> ContextCookie {
        ContextCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            secure: true,
            host_only: false,
            expires: Some(1_900_000_000),
        }
    }

    fn store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::in_dir(dir.path());
        (dir, store)
    }

    fn file_of(store: &SecretStore, id: &str) -> PathBuf {
        store.dir.join(format!("{id}.cookies"))
    }

    #[test]
    fn saved_cookies_load_back_unchanged() {
        let (_dir, store) = store();
        let cookies = vec![cookie("SID", "abc"), cookie("lang", "en")];
        store.save("dl_7", &cookies).unwrap();
        assert_eq!(store.load("dl_7").unwrap(), cookies);
    }

    #[test]
    fn the_file_starts_with_the_magic_version_and_scheme() {
        let (_dir, store) = store();
        store.save("dl_7", &[cookie("SID", "abc")]).unwrap();
        let bytes = std::fs::read(file_of(&store, "dl_7")).unwrap();
        assert_eq!(&bytes[..4], b"RDMC");
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], seal::SCHEME);
    }

    #[test]
    fn saving_again_replaces_the_cookies() {
        let (_dir, store) = store();
        store.save("dl_7", &[cookie("SID", "old")]).unwrap();
        store.save("dl_7", &[cookie("SID", "new")]).unwrap();
        assert_eq!(store.load("dl_7").unwrap()[0].value, "new");
    }

    #[test]
    fn a_missing_secret_is_reported_as_missing() {
        let (_dir, store) = store();
        assert!(matches!(store.load("dl_1"), Err(SecretError::Missing)));
    }

    #[test]
    fn damaged_files_are_reported_as_corrupt() {
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("SID", "abc")]).unwrap();
        let good = std::fs::read(file_of(&store, "dl_1")).unwrap();
        let damaged: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"RDM".to_vec(),
            [b"XXXX".as_slice(), &good[4..]].concat(),
            [&good[..4], &[9], &good[5..]].concat(),
            [&good[..5], &[77], &good[6..]].concat(),
        ];
        for bytes in damaged {
            std::fs::write(file_of(&store, "dl_1"), &bytes).unwrap();
            let err = store.load("dl_1").unwrap_err();
            assert!(matches!(err, SecretError::Corrupt), "{err:?}");
        }
    }

    #[test]
    fn a_file_that_does_not_hold_json_is_corrupt() {
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("SID", "abc")]).unwrap();
        let sealed = seal::seal(b"not json", &entropy_for("dl_1")).unwrap();
        let file = [b"RDMC".as_slice(), &[1, seal::SCHEME], &sealed].concat();
        std::fs::write(file_of(&store, "dl_1"), file).unwrap();
        assert!(matches!(store.load("dl_1"), Err(SecretError::Corrupt)));
    }

    #[test]
    fn a_file_sealed_by_another_scheme_cannot_be_decrypted_here() {
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("SID", "abc")]).unwrap();
        let other_scheme = if seal::SCHEME == 1 { 2 } else { 1 };
        let mut bytes = std::fs::read(file_of(&store, "dl_1")).unwrap();
        bytes[5] = other_scheme;
        std::fs::write(file_of(&store, "dl_1"), bytes).unwrap();
        assert!(matches!(
            store.load("dl_1"),
            Err(SecretError::Undecryptable)
        ));
    }

    #[test]
    fn ids_that_are_not_download_ids_never_reach_the_file_system() {
        let (dir, store) = store();
        for id in [
            "",
            "dl_",
            "dl_x",
            "../dl_1",
            "dl_1/../x",
            "dl_1.cookies",
            "DL_1",
            "dl_-1",
        ] {
            assert!(store.save(id, &[cookie("a", "b")]).is_err(), "{id:?}");
            assert!(matches!(store.load(id), Err(SecretError::Missing)));
            store.delete(id);
        }
        assert!(!dir.path().join("secrets").exists());
    }

    #[test]
    fn delete_removes_the_secret_and_its_temp_file_and_is_repeatable() {
        let (_dir, store) = store();
        store.save("dl_3", &[cookie("a", "b")]).unwrap();
        let leftover = temp_of(&file_of(&store, "dl_3"));
        std::fs::write(&leftover, b"half").unwrap();

        store.delete("dl_3");
        store.delete("dl_3");

        assert!(!file_of(&store, "dl_3").exists());
        assert!(!leftover.exists());
    }

    #[test]
    fn sweep_removes_orphans_and_temp_files_but_keeps_live_secrets() {
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("a", "b")]).unwrap();
        store.save("dl_2", &[cookie("a", "b")]).unwrap();
        std::fs::write(store.dir.join("dl_9.cookies.tmp"), b"half").unwrap();
        std::fs::write(store.dir.join("readme.txt"), b"mine").unwrap();

        store.sweep(&HashSet::from(["dl_1".to_string()]));

        assert!(file_of(&store, "dl_1").exists());
        assert!(!file_of(&store, "dl_2").exists());
        assert!(!store.dir.join("dl_9.cookies.tmp").exists());
        assert!(store.dir.join("readme.txt").exists());
    }

    #[test]
    fn sweeping_a_missing_folder_is_fine() {
        let (_dir, store) = store();
        store.sweep(&HashSet::new());
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_the_folder_is_0700_and_the_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("a", "b")]).unwrap();
        let folder = std::fs::metadata(&store.dir).unwrap().permissions().mode();
        let file = std::fs::metadata(file_of(&store, "dl_1"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(folder & 0o777, 0o700);
        assert_eq!(file & 0o777, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_file_does_not_hold_the_cookie_in_the_clear() {
        let (_dir, store) = store();
        store
            .save("dl_1", &[cookie("SID", "very-recognisable-value")])
            .unwrap();
        let bytes = std::fs::read(file_of(&store, "dl_1")).unwrap();
        assert!(!bytes
            .windows(b"recognisable".len())
            .any(|w| w == b"recognisable"));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_secret_copied_onto_another_download_does_not_open() {
        let (_dir, store) = store();
        store.save("dl_1", &[cookie("SID", "abc")]).unwrap();
        std::fs::copy(file_of(&store, "dl_1"), file_of(&store, "dl_2")).unwrap();
        assert!(matches!(
            store.load("dl_2"),
            Err(SecretError::Undecryptable)
        ));
    }

    #[test]
    fn download_ids_match_dl_and_digits_only() {
        assert!(is_download_id("dl_1"));
        assert!(is_download_id("dl_1234567"));
        assert!(!is_download_id("dl_"));
        assert!(!is_download_id("dl_1a"));
        assert!(!is_download_id("x_1"));
        assert!(!is_download_id(&format!("dl_{}", "9".repeat(21))));
    }
}
