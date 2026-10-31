use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub struct PartFile {
    pub file: File,
    pub path: PathBuf,
}

impl PartFile {
    pub fn create(part_path: &Path, total: Option<u64>) -> io::Result<PartFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(part_path)?;

        if let Some(len) = total {
            file.set_len(len)?;
        }

        Ok(PartFile {
            file,
            path: part_path.to_path_buf(),
        })
    }

    pub fn open(part_path: &Path) -> io::Result<PartFile> {
        let file = OpenOptions::new().read(true).write(true).open(part_path)?;

        Ok(PartFile {
            file,
            path: part_path.to_path_buf(),
        })
    }

    pub fn write_at(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
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
