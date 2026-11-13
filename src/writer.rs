use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// On NTFS, writing past the end of the data already written makes Windows
/// fill everything before it with zeros first. With several connections, the
/// first write near the end of a large file would wait for gigabytes of zeros
/// (measured: about 0.2-0.5 s per 384 MB). A sparse file skips that. Where
/// sparse files are not supported (FAT32, exFAT) this does nothing and the
/// file behaves as before.
#[cfg(windows)]
fn mark_sparse(file: &File) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            in_buffer: *mut c_void,
            in_size: u32,
            out_buffer: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    let mut returned = 0u32;
    // SAFETY: the handle is open for the lifetime of `file`; FSCTL_SET_SPARSE
    // takes no buffers; `returned` is a valid out-pointer; no overlapped I/O.
    unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
}

pub struct PartFile {
    pub file: File,
    pub path: PathBuf,
    /// Held for each write. Connections writing into one file at once make
    /// the kernel spin on the file's own lock: measured on a 1 GiB download
    /// to tmpfs with 8 connections, 4.7-6.6 s of system CPU and 0.75-2.0 s
    /// wall without this, 1.3 s and a steady 0.8 s with it. Taking turns here
    /// costs nothing when the disk is the limit.
    write_lock: Mutex<()>,
}

impl PartFile {
    pub fn create(part_path: &Path, total: Option<u64>) -> io::Result<PartFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(part_path)?;

        if let Some(len) = total {
            #[cfg(windows)]
            mark_sparse(&file);
            file.set_len(len)?;
        }

        Ok(PartFile {
            file,
            path: part_path.to_path_buf(),
            write_lock: Mutex::new(()),
        })
    }

    pub fn open(part_path: &Path) -> io::Result<PartFile> {
        let file = OpenOptions::new().read(true).write(true).open(part_path)?;

        Ok(PartFile {
            file,
            path: part_path.to_path_buf(),
            write_lock: Mutex::new(()),
        })
    }

    /// Write all of `data` at `offset`, one write at a time per file.
    pub fn write_at(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        // A panic mid-write leaves nothing for the lock to protect.
        let _turn = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            while !data.is_empty() {
                let n = self.file.write_at(data, offset)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write any bytes",
                    ));
                }
                offset += n as u64;
                data = &data[n..];
            }
            Ok(())
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            while !data.is_empty() {
                let n = self.file.seek_write(data, offset)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write any bytes",
                    ));
                }
                offset += n as u64;
                data = &data[n..];
            }
            Ok(())
        }

        #[cfg(not(any(unix, windows)))]
        {
            compile_error!("unsupported target platform");
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn finish(self, final_path: &Path) -> io::Result<()> {
        self.sync()?;
        if final_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination file already exists",
            ));
        }
        std::fs::rename(&self.path, final_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_file_gets_its_full_length_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part");
        let part = PartFile::create(&path, Some(8 * 1024 * 1024)).unwrap();
        part.write_at(8 * 1024 * 1024 - 4, b"tail").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8 * 1024 * 1024);
    }

    #[test]
    fn writes_from_many_threads_at_once_all_land_in_place() {
        const PIECE: usize = 256 * 1024;
        const THREADS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part");
        let part =
            std::sync::Arc::new(PartFile::create(&path, Some((PIECE * THREADS) as u64)).unwrap());
        let writers: Vec<_> = (0..THREADS)
            .map(|i| {
                let part = std::sync::Arc::clone(&part);
                std::thread::spawn(move || {
                    for chunk in 0..PIECE / 4096 {
                        let offset = i * PIECE + chunk * 4096;
                        part.write_at(offset as u64, &[i as u8 + 1; 4096]).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let written = std::fs::read(&path).unwrap();
        for (i, piece) in written.chunks(PIECE).enumerate() {
            assert!(piece.iter().all(|&b| b == i as u8 + 1), "piece {i}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_part_file_is_sparse() {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part");
        PartFile::create(&path, Some(1024 * 1024)).unwrap();
        let attributes = std::fs::metadata(&path).unwrap().file_attributes();
        assert_ne!(attributes & FILE_ATTRIBUTE_SPARSE_FILE, 0);
    }
}
