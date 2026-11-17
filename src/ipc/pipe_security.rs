//! The security descriptor for the daemon's named pipe. Left to default, a
//! pipe made by an elevated process refuses a client that is not elevated
//! ("Access is denied"); this names exactly who may connect instead: the
//! current user and the system, whether or not the process is elevated.

/// The descriptor as SDDL: a protected DACL (no inherited entries) that gives
/// full access to `user_sid` and to the local system (`SY`), and nobody else.
pub fn sddl_for_user(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)")
}

#[cfg(windows)]
pub use windows::PipeSecurity;

#[cfg(windows)]
mod windows {
    use super::sddl_for_user;
    use std::ffi::c_void;
    use std::io;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_descriptor: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            descriptor_size: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// A security descriptor to give every instance of the daemon's pipe.
    pub struct PipeSecurity {
        descriptor: *mut c_void,
    }

    // SAFETY: the descriptor is immutable memory owned by this value and only
    // read by the pipe-creation call.
    unsafe impl Send for PipeSecurity {}
    unsafe impl Sync for PipeSecurity {}

    impl PipeSecurity {
        /// Full access for the current user and the system only.
        pub fn for_current_user() -> io::Result<Self> {
            Self::from_sddl(&sddl_for_user(
                &crate::ipc::owner::current_user_sid_string()?
            ))
        }

        pub fn from_sddl(sddl: &str) -> io::Result<Self> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated; `descriptor` is a valid out-pointer.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }

        /// Create one pipe instance with this descriptor. Every instance needs
        /// it, not just the first, or later ones get the default access.
        pub fn create_server(
            &self,
            first_instance: bool,
            name: &str,
        ) -> io::Result<NamedPipeServer> {
            let mut attributes = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                descriptor: self.descriptor,
                inherit_handle: 0,
            };
            let mut options = ServerOptions::new();
            options.first_pipe_instance(first_instance);
            // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES for the whole
            // call, and its descriptor stays alive as long as `self`.
            unsafe {
                options.create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SecurityAttributes).cast(),
                )
            }
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            // SAFETY: the descriptor was allocated by the conversion call
            // (LocalAlloc) and is freed once.
            unsafe { LocalFree(self.descriptor) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::io::AsRawHandle;
        use tokio::net::windows::named_pipe::ClientOptions;

        const SE_KERNEL_OBJECT: u32 = 6;
        const DACL_SECURITY_INFORMATION: u32 = 4;

        #[link(name = "advapi32")]
        extern "system" {
            fn GetSecurityInfo(
                handle: *mut c_void,
                object_type: u32,
                information: u32,
                owner: *mut *mut c_void,
                group: *mut *mut c_void,
                dacl: *mut *mut c_void,
                sacl: *mut *mut c_void,
                descriptor: *mut *mut c_void,
            ) -> u32;
            fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor: *mut c_void,
                revision: u32,
                information: u32,
                string_descriptor: *mut *mut u16,
                string_len: *mut u32,
            ) -> i32;
        }

        /// The DACL inside `descriptor` as SDDL text. The caller keeps
        /// ownership of `descriptor`.
        fn dacl_text(descriptor: *mut c_void) -> String {
            let mut text: *mut u16 = std::ptr::null_mut();
            // SAFETY: `descriptor` is a valid security descriptor; `text` is a
            // valid out-pointer.
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "conversion to SDDL failed");
            // SAFETY: `text` is a NUL-terminated UTF-16 string in LocalAlloc
            // memory, freed once.
            unsafe {
                let mut len = 0;
                while *text.add(len) != 0 {
                    len += 1;
                }
                let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
                sddl
            }
        }

        /// The DACL of `pipe` as SDDL text.
        fn dacl_of(pipe: &NamedPipeServer) -> String {
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            // SAFETY: valid pipe handle; only the descriptor out-pointer is requested.
            let code = unsafe {
                GetSecurityInfo(
                    pipe.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(code, 0, "GetSecurityInfo failed");
            let sddl = dacl_text(descriptor);
            // SAFETY: `descriptor` came from GetSecurityInfo (LocalAlloc) and is freed once.
            unsafe { LocalFree(descriptor) };
            sddl
        }

        /// How Windows writes `sid` as an SDDL trustee. Some well-known SIDs are
        /// abbreviated to an alias (the built-in Administrator, RID 500, is
        /// `LA`), so the raw SID string cannot be compared to a DACL as read
        /// back. Round-trip it through a descriptor the same way instead.
        fn trustee_as_windows_writes_it(sid: &str) -> String {
            let wide: Vec<u16> = format!("D:(A;;GA;;;{sid})")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated; `descriptor` is a valid out-pointer.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "the user's SID did not convert");
            let sddl = dacl_text(descriptor);
            // SAFETY: `descriptor` was allocated by the conversion (LocalAlloc), freed once.
            unsafe { LocalFree(descriptor) };
            let (_, tail) = sddl.rsplit_once(';').expect("an entry with a trustee");
            tail.split(')').next().unwrap_or_default().to_string()
        }

        #[tokio::test]
        async fn a_pipe_made_with_it_can_be_opened_by_the_same_user() {
            let name = format!(r"\\.\pipe\rustdm_sec_{}", std::process::id());
            let security = PipeSecurity::for_current_user().unwrap();
            let server = security.create_server(true, &name).unwrap();
            let client = ClientOptions::new()
                .open(&name)
                .expect("same user can open it");
            drop(client);
            drop(server);
        }

        #[tokio::test]
        async fn the_dacl_names_only_the_current_user_and_the_system() {
            let name = format!(r"\\.\pipe\rustdm_dacl_{}", std::process::id());
            let security = PipeSecurity::for_current_user().unwrap();
            let first = security.create_server(true, &name).unwrap();
            // A later instance gets the same protection as the first.
            let second = security.create_server(false, &name).unwrap();

            let user = trustee_as_windows_writes_it(
                &crate::ipc::owner::current_user_sid_string().unwrap(),
            );
            for pipe in [&first, &second] {
                let dacl = dacl_of(pipe);
                assert!(dacl.starts_with("D:P"), "protected DACL: {dacl}");
                assert_eq!(dacl.matches("(A;").count(), 2, "two entries: {dacl}");
                assert!(dacl.contains(&format!(";{user})")), "user entry: {dacl}");
                assert!(dacl.contains(";SY)"), "system entry: {dacl}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sddl_protects_the_dacl_and_names_the_user_and_the_system() {
        assert_eq!(
            sddl_for_user("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)"
        );
    }
}
