//! Registering the host with browsers: the host manifest, and where each
//! browser looks for it (a registry key on Windows, a folder on Linux).

use crate::nmhost::origin::ALLOWED_EXTENSION_IDS;
use crate::state::write_atomic;
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};

/// Name the extension passes to `sendNativeMessage`.
pub const HOST_NAME: &str = "rustdm.nmhost";
const HOST_DESCRIPTION: &str = "rustdm download manager";
const MANIFEST_FILE: &str = "rustdm.nmhost.json";

/// One browser's registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserEntry {
    pub browser: &'static str,
    /// Where the registration lives: a manifest path or a registry key.
    pub location: String,
    pub registered: bool,
    /// The host program the registration points at, when it could be read.
    pub host_path: Option<PathBuf>,
}

/// The host manifest. One manifest lists every allowed extension, because Edge
/// reads the first manifest it finds.
pub fn manifest_value(host_exe: &Path) -> Value {
    let origins: Vec<String> = ALLOWED_EXTENSION_IDS
        .iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect();
    json!({
        "name": HOST_NAME,
        "description": HOST_DESCRIPTION,
        "path": host_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": origins,
    })
}

fn manifest_bytes(host_exe: &Path) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&manifest_value(host_exe)).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// The `path` a manifest file points at, if the file is a readable manifest.
fn host_path_in(manifest: &Path) -> Option<PathBuf> {
    let value: Value = serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
    value.get("path")?.as_str().map(PathBuf::from)
}

fn write_manifest(manifest: &Path, host_exe: &Path) -> io::Result<()> {
    if let Some(dir) = manifest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(manifest, &manifest_bytes(host_exe))
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Path of the `rdm-nmhost` program installed next to the running binary.
pub fn host_exe_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable has no parent directory",
        )
    })?;
    Ok(dir.join(format!("rdm-nmhost{}", std::env::consts::EXE_SUFFIX)))
}

/// One line for `rdm browser status`. `expected_host` is where the host program is now.
pub fn describe(entry: &BrowserEntry, expected_host: &Path) -> String {
    if !entry.registered {
        return format!("{}: not registered", entry.browser);
    }
    match &entry.host_path {
        Some(path) if path == expected_host => {
            format!("{}: registered ({})", entry.browser, entry.location)
        }
        Some(path) => format!(
            "{}: registered, but points at {}; run `rdm browser install` to fix it",
            entry.browser,
            path.display()
        ),
        None => format!(
            "{}: registered, but its manifest cannot be read; run `rdm browser install`",
            entry.browser
        ),
    }
}

/// Register `host_exe` with every supported browser found. Safe to repeat.
pub fn install(host_exe: &Path) -> io::Result<Vec<BrowserEntry>> {
    platform::install(host_exe)
}

/// What `uninstall` did. One browser failing does not stop the others.
#[derive(Debug, Default)]
pub struct UninstallReport {
    /// The registrations that existed and are gone.
    pub removed: Vec<BrowserEntry>,
    /// What could not be done, one line each.
    pub problems: Vec<String>,
}

/// Remove the registrations.
pub fn uninstall() -> io::Result<UninstallReport> {
    platform::uninstall()
}

/// What is registered now.
pub fn status() -> io::Result<Vec<BrowserEntry>> {
    platform::status()
}

#[cfg(unix)]
mod platform {
    use super::*;

    /// Browser name and its folder under the config directory.
    pub(super) const BROWSERS: [(&str, &str); 5] = [
        ("Google Chrome", "google-chrome"),
        ("Chromium", "chromium"),
        ("Microsoft Edge", "microsoft-edge"),
        ("Brave", "BraveSoftware/Brave-Browser"),
        ("Vivaldi", "vivaldi"),
    ];

    pub(super) fn config_home() -> io::Result<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        crate::dirs::home_dir()
            .map(|home| home.join(".config"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder is set"))
    }

    fn manifest_path(config_home: &Path, folder: &str) -> PathBuf {
        config_home
            .join(folder)
            .join("NativeMessagingHosts")
            .join(MANIFEST_FILE)
    }

    fn entry(config_home: &Path, browser: &'static str, folder: &str) -> BrowserEntry {
        let manifest = manifest_path(config_home, folder);
        BrowserEntry {
            browser,
            registered: manifest.is_file(),
            host_path: host_path_in(&manifest),
            location: manifest.display().to_string(),
        }
    }

    pub(super) fn install_in(config_home: &Path, host_exe: &Path) -> io::Result<Vec<BrowserEntry>> {
        let mut done = Vec::new();
        for (browser, folder) in BROWSERS {
            // Only browsers that have been run: their config folder exists.
            if !config_home.join(folder).is_dir() {
                continue;
            }
            write_manifest(&manifest_path(config_home, folder), host_exe)?;
            done.push(entry(config_home, browser, folder));
        }
        Ok(done)
    }

    pub(super) fn uninstall_in(config_home: &Path) -> UninstallReport {
        let mut report = UninstallReport::default();
        for (browser, folder) in BROWSERS {
            let before = entry(config_home, browser, folder);
            match remove_if_present(&manifest_path(config_home, folder)) {
                Ok(true) => report.removed.push(before),
                Ok(false) => {}
                Err(e) => report
                    .problems
                    .push(format!("{browser}: cannot remove {}: {e}", before.location)),
            }
        }
        report
    }

    pub(super) fn status_in(config_home: &Path) -> Vec<BrowserEntry> {
        BROWSERS
            .iter()
            .filter(|(_, folder)| config_home.join(folder).is_dir())
            .map(|(browser, folder)| entry(config_home, browser, folder))
            .collect()
    }

    pub(super) fn install(host_exe: &Path) -> io::Result<Vec<BrowserEntry>> {
        install_in(&config_home()?, host_exe)
    }

    pub(super) fn uninstall() -> io::Result<UninstallReport> {
        Ok(uninstall_in(&config_home()?))
    }

    pub(super) fn status() -> io::Result<Vec<BrowserEntry>> {
        Ok(status_in(&config_home()?))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::nmhost::registry;

    /// Browser name and its vendor path under `Software`.
    const BROWSERS: [(&str, &str); 2] = [
        ("Google Chrome", r"Google\Chrome"),
        ("Microsoft Edge", r"Microsoft\Edge"),
    ];

    /// The registry root the keys go under; tests use a throwaway one.
    pub(super) const SOFTWARE_ROOT: &str = "Software";

    pub(super) fn manifest_path(local_app_data: &Path) -> PathBuf {
        local_app_data
            .join("rustdm")
            .join("nmhost")
            .join(MANIFEST_FILE)
    }

    pub(super) fn key_for(root: &str, vendor: &str) -> String {
        format!(r"{root}\{vendor}\NativeMessagingHosts\{HOST_NAME}")
    }

    fn local_app_data() -> io::Result<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))
    }

    fn entry(root: &str, browser: &'static str, vendor: &str) -> io::Result<BrowserEntry> {
        let key = key_for(root, vendor);
        let registered_manifest = registry::get_default(&key)?;
        Ok(BrowserEntry {
            browser,
            registered: registered_manifest.is_some(),
            host_path: registered_manifest
                .as_deref()
                .and_then(|m| host_path_in(Path::new(m))),
            location: format!(r"HKCU\{key}"),
        })
    }

    pub(super) fn install_in(
        local_app_data: &Path,
        root: &str,
        host_exe: &Path,
    ) -> io::Result<Vec<BrowserEntry>> {
        let manifest = manifest_path(local_app_data);
        write_manifest(&manifest, host_exe)?;
        let mut done = Vec::new();
        for (browser, vendor) in BROWSERS {
            registry::set_default(&key_for(root, vendor), &manifest.to_string_lossy())?;
            done.push(entry(root, browser, vendor)?);
        }
        Ok(done)
    }

    pub(super) fn uninstall_in(local_app_data: &Path, root: &str) -> UninstallReport {
        let mut report = UninstallReport::default();
        for (browser, vendor) in BROWSERS {
            // Reading the old state is only for the report; the key is deleted regardless.
            let before = entry(root, browser, vendor).unwrap_or_else(|e| {
                report
                    .problems
                    .push(format!("{browser}: cannot read its registration: {e}"));
                BrowserEntry {
                    browser,
                    location: format!(r"HKCU\{}", key_for(root, vendor)),
                    registered: true,
                    host_path: None,
                }
            });
            match registry::delete_key(&key_for(root, vendor)) {
                Ok(true) => report.removed.push(before),
                Ok(false) => {}
                Err(e) => report
                    .problems
                    .push(format!("{browser}: cannot remove {}: {e}", before.location)),
            }
        }
        let manifest = manifest_path(local_app_data);
        if let Err(e) = remove_if_present(&manifest) {
            report
                .problems
                .push(format!("cannot remove {}: {e}", manifest.display()));
        }
        // Leave the folder if anything else is in it.
        if let Some(dir) = manifest.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        report
    }

    pub(super) fn status_in(root: &str) -> io::Result<Vec<BrowserEntry>> {
        BROWSERS
            .iter()
            .map(|(browser, vendor)| entry(root, browser, vendor))
            .collect()
    }

    pub(super) fn install(host_exe: &Path) -> io::Result<Vec<BrowserEntry>> {
        install_in(&local_app_data()?, SOFTWARE_ROOT, host_exe)
    }

    pub(super) fn uninstall() -> io::Result<UninstallReport> {
        Ok(uninstall_in(&local_app_data()?, SOFTWARE_ROOT))
    }

    pub(super) fn status() -> io::Result<Vec<BrowserEntry>> {
        status_in(SOFTWARE_ROOT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_the_host_and_every_allowed_extension() {
        let value = manifest_value(Path::new("/opt/rustdm/rdm-nmhost"));
        assert_eq!(value["name"], "rustdm.nmhost");
        assert_eq!(value["type"], "stdio");
        assert_eq!(value["path"], "/opt/rustdm/rdm-nmhost");
        let origins: Vec<&str> = value["allowed_origins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let expected: Vec<String> = ALLOWED_EXTENSION_IDS
            .iter()
            .map(|id| format!("chrome-extension://{id}/"))
            .collect();
        assert_eq!(origins, expected);
    }

    #[test]
    fn status_lines_say_what_to_do_when_something_is_off() {
        let host = Path::new("/new/rdm-nmhost");
        let mut entry = BrowserEntry {
            browser: "Google Chrome",
            location: "/m.json".to_string(),
            registered: false,
            host_path: None,
        };
        assert_eq!(describe(&entry, host), "Google Chrome: not registered");
        entry.registered = true;
        assert!(describe(&entry, host).contains("cannot be read"));
        entry.host_path = Some(PathBuf::from("/old/rdm-nmhost"));
        let line = describe(&entry, host);
        assert!(line.contains("/old/rdm-nmhost") && line.contains("rdm browser install"));
        entry.host_path = Some(host.to_path_buf());
        assert_eq!(
            describe(&entry, host),
            "Google Chrome: registered (/m.json)"
        );
    }

    #[test]
    fn the_host_program_sits_next_to_the_running_binary() {
        let path = host_exe_path().unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("rdm-nmhost"));
    }

    #[cfg(unix)]
    mod linux {
        use super::super::platform::*;
        use super::*;

        const HOST: &str = "/opt/rustdm/rdm-nmhost";

        fn config_with(folders: &[&str]) -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            for folder in folders {
                std::fs::create_dir_all(dir.path().join(folder)).unwrap();
            }
            dir
        }

        fn manifest_of(config: &Path, folder: &str) -> PathBuf {
            config
                .join(folder)
                .join("NativeMessagingHosts")
                .join("rustdm.nmhost.json")
        }

        #[test]
        fn only_browsers_that_have_a_config_folder_are_registered() {
            let config = config_with(&["google-chrome", "microsoft-edge"]);
            let done = install_in(config.path(), Path::new(HOST)).unwrap();
            let names: Vec<_> = done.iter().map(|e| e.browser).collect();
            assert_eq!(names, ["Google Chrome", "Microsoft Edge"]);
            assert!(done.iter().all(|e| e.registered));
            assert!(manifest_of(config.path(), "google-chrome").is_file());
            assert!(manifest_of(config.path(), "microsoft-edge").is_file());
            assert!(!config.path().join("chromium").exists());
        }

        #[test]
        fn the_written_manifest_is_the_host_manifest() {
            let config = config_with(&["BraveSoftware/Brave-Browser"]);
            install_in(config.path(), Path::new(HOST)).unwrap();
            let text =
                std::fs::read_to_string(manifest_of(config.path(), "BraveSoftware/Brave-Browser"))
                    .unwrap();
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value, manifest_value(Path::new(HOST)));
        }

        #[test]
        fn installing_again_after_the_folder_moves_updates_the_path() {
            let config = config_with(&["chromium"]);
            install_in(config.path(), Path::new("/old/rdm-nmhost")).unwrap();
            install_in(config.path(), Path::new("/new/rdm-nmhost")).unwrap();
            let listed = status_in(config.path());
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].host_path, Some(PathBuf::from("/new/rdm-nmhost")));
        }

        #[test]
        fn status_lists_browsers_with_their_registration_state() {
            let config = config_with(&["google-chrome", "vivaldi"]);
            install_in(config.path(), Path::new(HOST)).unwrap();
            std::fs::remove_file(manifest_of(config.path(), "vivaldi")).unwrap();
            let listed = status_in(config.path());
            assert_eq!(listed.len(), 2);
            assert!(listed[0].registered && listed[0].browser == "Google Chrome");
            assert!(!listed[1].registered && listed[1].browser == "Vivaldi");
            assert_eq!(listed[1].host_path, None);
        }

        #[test]
        fn uninstall_removes_the_manifests_and_reports_what_it_removed() {
            let config = config_with(&["google-chrome", "chromium"]);
            install_in(config.path(), Path::new(HOST)).unwrap();
            let report = uninstall_in(config.path());
            assert_eq!(report.removed.len(), 2);
            assert!(report.problems.is_empty());
            assert!(!manifest_of(config.path(), "google-chrome").exists());
            assert!(status_in(config.path()).iter().all(|e| !e.registered));
            assert!(uninstall_in(config.path()).removed.is_empty());
        }

        #[test]
        fn one_browser_failing_does_not_stop_the_others() {
            let config = config_with(&["google-chrome", "chromium", "vivaldi"]);
            install_in(config.path(), Path::new(HOST)).unwrap();
            // Make Chromium's manifest impossible to remove as a file.
            let stuck = manifest_of(config.path(), "chromium");
            std::fs::remove_file(&stuck).unwrap();
            std::fs::create_dir(&stuck).unwrap();

            let report = uninstall_in(config.path());
            let removed: Vec<_> = report.removed.iter().map(|e| e.browser).collect();
            assert_eq!(removed, ["Google Chrome", "Vivaldi"]);
            assert_eq!(report.problems.len(), 1);
            assert!(
                report.problems[0].starts_with("Chromium:"),
                "{:?}",
                report.problems
            );
            assert!(!manifest_of(config.path(), "vivaldi").exists());
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::super::platform::*;
        use super::*;
        use crate::nmhost::registry;

        /// A registry root that only this test run uses.
        struct ThrowawayRoot(String);

        impl ThrowawayRoot {
            fn new() -> Self {
                Self(format!(r"Software\rustdm-test-{}", std::process::id()))
            }
        }

        impl Drop for ThrowawayRoot {
            fn drop(&mut self) {
                let root = &self.0;
                for vendor in [r"Google\Chrome", r"Microsoft\Edge"] {
                    let host = key_for(root, vendor);
                    let mut key = host.as_str();
                    // Delete the host key, then each parent up to the root.
                    loop {
                        let _ = registry::delete_key(key);
                        match key.rfind('\\') {
                            Some(at) if key.len() > root.len() => key = &key[..at],
                            _ => break,
                        }
                    }
                }
                let _ = registry::delete_key(root);
            }
        }

        #[test]
        fn registry_round_trip_under_a_throwaway_key() {
            let root = ThrowawayRoot::new();
            let appdata = tempfile::tempdir().unwrap();
            let host = Path::new(r"C:\rustdm\rdm-nmhost.exe");

            let done = install_in(appdata.path(), &root.0, host).unwrap();
            assert_eq!(done.len(), 2);
            assert!(done.iter().all(|e| e.registered));
            assert!(done.iter().all(|e| e.host_path.as_deref() == Some(host)));
            assert!(manifest_path(appdata.path()).is_file());

            // Repeating is harmless.
            install_in(appdata.path(), &root.0, host).unwrap();
            assert!(status_in(&root.0).unwrap().iter().all(|e| e.registered));

            let report = uninstall_in(appdata.path(), &root.0);
            assert_eq!(report.removed.len(), 2);
            assert!(report.problems.is_empty(), "{:?}", report.problems);
            assert!(status_in(&root.0).unwrap().iter().all(|e| !e.registered));
            assert!(!manifest_path(appdata.path()).exists());
        }

        #[test]
        fn a_missing_key_reads_as_none() {
            let key = format!(r"Software\rustdm-test-missing-{}", std::process::id());
            assert_eq!(registry::get_default(&key).unwrap(), None);
            assert!(!registry::delete_key(&key).unwrap());
        }
    }
}
