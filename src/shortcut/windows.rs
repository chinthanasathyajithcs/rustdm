//! Windows `.lnk` files through the Shell's `IShellLinkW` and `IPersistFile`,
//! by raw COM calls (the style of `src/nmhost/registry.rs`: no extra crates).
//!
//! COM needs an initialised thread, so every public call runs its work on a
//! short-lived thread of its own and leaves the caller's thread untouched.

use super::{link_in, Found, Place, Target, DESCRIPTION};
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const CLSID_SHELL_LINK: Guid = Guid {
    data1: 0x0002_1401,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};
const IID_ISHELL_LINK_W: Guid = Guid {
    data1: 0x0002_14F9,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};
const IID_IPERSIST_FILE: Guid = Guid {
    data1: 0x0000_010B,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
};
const FOLDERID_DESKTOP: Guid = Guid {
    data1: 0xB4BF_CC3A,
    data2: 0xDB2C,
    data3: 0x424C,
    data4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
};
const FOLDERID_PROGRAMS: Guid = Guid {
    data1: 0xA77F_5D77,
    data2: 0x2E2B,
    data3: 0x44C3,
    data4: [0xA6, 0xA2, 0xAB, 0xA6, 0x01, 0x05, 0x4A, 0x51],
};

type HResult = i32;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const CLSCTX_INPROC_SERVER: u32 = 0x1;
const STGM_READ: u32 = 0;
const RPC_E_CHANGED_MODE: HResult = 0x8001_0106_u32 as i32;
const PATH_CHARS: usize = 4096;

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> HResult;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        context: u32,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> HResult;
    fn CoTaskMemFree(memory: *mut c_void);
}

#[link(name = "shell32")]
extern "system" {
    fn SHGetKnownFolderPath(
        folder: *const Guid,
        flags: u32,
        token: *mut c_void,
        path: *mut *mut u16,
    ) -> HResult;
}

/// The start of `IShellLinkW`'s table, up to the last method used (`SetPath`).
#[repr(C)]
struct ShellLinkVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_path: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> HResult,
    get_id_list: usize,
    set_id_list: usize,
    get_description: usize,
    set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
    get_working_directory: usize,
    set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
    get_arguments: usize,
    set_arguments: usize,
    get_hotkey: usize,
    set_hotkey: usize,
    get_show_cmd: usize,
    set_show_cmd: usize,
    get_icon_location: usize,
    set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HResult,
    set_relative_path: usize,
    resolve: usize,
    set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
}

/// `IPersistFile`'s table, up to `Save`.
#[repr(C)]
struct PersistFileVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_class_id: usize,
    is_dirty: usize,
    load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HResult,
    save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HResult,
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn check(hr: HResult, what: &str) -> io::Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{what} failed (0x{:08X})",
            hr as u32
        )))
    }
}

/// A shell link object with its file interface; both released on drop.
struct Link {
    link: *mut c_void,
    file: *mut c_void,
}

impl Link {
    fn new() -> io::Result<Link> {
        let mut link = null_mut();
        // SAFETY: valid GUID pointers and an out pointer; COM is initialised on
        // this thread by `on_com_thread`.
        check(
            unsafe {
                CoCreateInstance(
                    &CLSID_SHELL_LINK,
                    null_mut(),
                    CLSCTX_INPROC_SERVER,
                    &IID_ISHELL_LINK_W,
                    &mut link,
                )
            },
            "creating a shell link",
        )?;
        let mut file = null_mut();
        // SAFETY: `link` is a live IShellLinkW; QueryInterface writes `file`.
        let hr =
            unsafe { ((*Self::table(link)).query_interface)(link, &IID_IPERSIST_FILE, &mut file) };
        if let Err(e) = check(hr, "opening the shell link as a file") {
            // SAFETY: release the one reference CoCreateInstance gave us.
            unsafe { ((*Self::table(link)).release)(link) };
            return Err(e);
        }
        Ok(Link { link, file })
    }

    fn table(link: *mut c_void) -> *const ShellLinkVtbl {
        // SAFETY (for callers): a COM object starts with its table pointer.
        unsafe { *(link as *const *const ShellLinkVtbl) }
    }

    fn links(&self) -> &ShellLinkVtbl {
        // SAFETY: `self.link` stays alive until drop.
        unsafe { &*Self::table(self.link) }
    }

    fn files(&self) -> &PersistFileVtbl {
        // SAFETY: `self.file` stays alive until drop; it starts with its table.
        unsafe { &**(self.file as *const *const PersistFileVtbl) }
    }

    fn write(&self, target: &Target, at: &Path) -> io::Result<()> {
        let exe = wide(&target.exe);
        let dir = wide(&target.working_dir);
        let description: Vec<u16> = DESCRIPTION.encode_utf16().chain([0]).collect();
        let at = wide(at);
        let l = self.links();
        // SAFETY: every string is NUL-terminated and outlives its call.
        unsafe {
            check((l.set_path)(self.link, exe.as_ptr()), "setting the target")?;
            check(
                (l.set_working_directory)(self.link, dir.as_ptr()),
                "setting the start folder",
            )?;
            check(
                (l.set_description)(self.link, description.as_ptr()),
                "setting the description",
            )?;
            check(
                (l.set_icon_location)(self.link, exe.as_ptr(), 0),
                "setting the icon",
            )?;
            check(
                (self.files().save)(self.file, at.as_ptr(), 1),
                "saving the shortcut",
            )
        }
    }

    /// What an existing shortcut file points at.
    fn read_target(&self, at: &Path) -> io::Result<PathBuf> {
        let at = wide(at);
        let mut buf = vec![0u16; PATH_CHARS];
        // SAFETY: `at` is NUL-terminated; `buf` holds PATH_CHARS characters and
        // GetPath writes at most that many, NUL included; the find data is optional.
        unsafe {
            check(
                (self.files().load)(self.file, at.as_ptr(), STGM_READ),
                "reading the shortcut",
            )?;
            check(
                (self.links().get_path)(
                    self.link,
                    buf.as_mut_ptr(),
                    PATH_CHARS as i32,
                    null_mut(),
                    0,
                ),
                "reading the shortcut's target",
            )?;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // SAFETY: each pointer holds exactly one reference we own.
        unsafe {
            (self.files().release)(self.file);
            (self.links().release)(self.link);
        }
    }
}

/// Run `work` on a fresh thread with COM initialised for it.
fn on_com_thread<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    std::thread::spawn(move || {
        // SAFETY: plain initialisation of this new thread's COM apartment.
        let hr = unsafe { CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED) };
        if hr < 0 && hr != RPC_E_CHANGED_MODE {
            return Err(io::Error::other(format!(
                "starting COM failed (0x{:08X})",
                hr as u32
            )));
        }
        let result = work();
        if hr >= 0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result
    })
    .join()
    .map_err(|_| io::Error::other("the shortcut worker stopped unexpectedly"))?
}

fn known_folder(id: &Guid) -> io::Result<PathBuf> {
    let mut raw: *mut u16 = null_mut();
    // SAFETY: `id` is a valid folder id; on success `raw` is a NUL-terminated
    // string that we free with CoTaskMemFree (also required on failure).
    let hr = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        let mut len = 0;
        // SAFETY: walk to the terminating NUL of the shell's string.
        while unsafe { *raw.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` characters were just checked.
        let chars = unsafe { std::slice::from_raw_parts(raw, len) };
        Ok(PathBuf::from(String::from_utf16_lossy(chars)))
    } else {
        Err(io::Error::other(format!(
            "cannot find the folder (0x{:08X})",
            hr as u32
        )))
    };
    // SAFETY: CoTaskMemFree accepts null.
    unsafe { CoTaskMemFree(raw as *mut c_void) };
    path
}

fn folder_of(place: Place) -> io::Result<PathBuf> {
    match place {
        Place::Desktop => known_folder(&FOLDERID_DESKTOP),
        Place::StartMenu => known_folder(&FOLDERID_PROGRAMS),
    }
}

fn places() -> io::Result<Vec<(Place, PathBuf)>> {
    Place::ALL
        .iter()
        .map(|&place| folder_of(place).map(|dir| (place, dir)))
        .collect()
}

pub(super) fn install_in(
    folders: Vec<(Place, PathBuf)>,
    target: Target,
) -> io::Result<Vec<(Place, PathBuf)>> {
    on_com_thread(move || {
        let mut written = Vec::new();
        for (place, dir) in folders {
            let link = link_in(&dir);
            Link::new()?.write(&target, &link)?;
            written.push((place, link));
        }
        Ok(written)
    })
}

pub(super) fn status_in(folders: Vec<(Place, PathBuf)>) -> io::Result<Vec<Found>> {
    on_com_thread(move || {
        let mut found = Vec::new();
        for (place, dir) in folders {
            let link = link_in(&dir);
            let points_to = if link.is_file() {
                Some(Link::new()?.read_target(&link).unwrap_or_default())
            } else {
                None
            };
            found.push(Found {
                place,
                link,
                points_to,
            });
        }
        Ok(found)
    })
}

pub(super) fn remove_in(folders: Vec<(Place, PathBuf)>) -> io::Result<Vec<(Place, PathBuf)>> {
    let mut removed = Vec::new();
    for (place, dir) in folders {
        let link = link_in(&dir);
        match std::fs::remove_file(&link) {
            Ok(()) => removed.push((place, link)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("cannot remove {}: {e}", link.display()),
                ))
            }
        }
    }
    Ok(removed)
}

pub(super) fn install(target: &Target) -> io::Result<Vec<(Place, PathBuf)>> {
    install_in(places()?, target.clone())
}

pub(super) fn remove() -> io::Result<Vec<(Place, PathBuf)>> {
    remove_in(places()?)
}

pub(super) fn status() -> io::Result<Vec<Found>> {
    status_in(places()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths compared the way Windows resolves them: a short `RUNNER~1` and
    /// the long name it stands for are the same folder.
    fn same_file(a: &Path, b: &Path) -> bool {
        std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
    }

    fn temp_place(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustdm-shortcut-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_shortcut_is_written_read_back_and_removed() {
        let dir = temp_place("roundtrip");
        let exe = std::env::current_exe().unwrap();
        let target = Target::for_exe(&exe);
        let folders = vec![(Place::Desktop, dir.clone())];

        let written = install_in(folders.clone(), target).unwrap();
        assert_eq!(written, vec![(Place::Desktop, dir.join("rustdm.lnk"))]);
        assert!(dir.join("rustdm.lnk").is_file());

        let found = status_in(folders.clone()).unwrap();
        assert!(same_file(found[0].points_to.as_deref().unwrap(), &exe));

        let removed = remove_in(folders.clone()).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(status_in(folders.clone()).unwrap()[0].points_to, None);
        assert!(
            remove_in(folders).unwrap().is_empty(),
            "removing twice is quiet"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installing_again_replaces_the_shortcut() {
        let dir = temp_place("replace");
        let folders = vec![(Place::StartMenu, dir.clone())];
        let first = std::env::current_exe().unwrap();
        install_in(folders.clone(), Target::for_exe(&first)).unwrap();
        let other = dir.join("elsewhere").join("rdm.exe");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"").unwrap();
        install_in(folders.clone(), Target::for_exe(&other)).unwrap();
        let found = status_in(folders).unwrap();
        assert!(same_file(found[0].points_to.as_deref().unwrap(), &other));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_desktop_and_start_menu_folders_are_found() {
        for (place, dir) in places().unwrap() {
            assert!(dir.is_absolute(), "{}: {}", place.label(), dir.display());
        }
    }
}
