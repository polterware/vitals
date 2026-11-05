//! A `/proc` file kept open and read again from offset 0 into a reused buffer, so a sample
//! costs one or two syscalls and no allocation. Some proc files reject positioned reads; those
//! fall back to seek and read on the same descriptor.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;

const MAX_READ: usize = 4 << 20;

pub struct ProcFile {
    file: File,
    path: String,
    buf: Vec<u8>,
    no_pread: bool,
}

impl ProcFile {
    pub fn open(path: &str) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            path: path.to_string(),
            buf: vec![0; 8192],
            no_pread: false,
        })
    }

    /// The whole file as it reads now. The slice is valid until the next read.
    pub fn read(&mut self) -> io::Result<&[u8]> {
        let mut filled = 0;
        loop {
            if filled == self.buf.len() {
                if filled >= MAX_READ {
                    return Err(io::Error::other(format!(
                        "{}: proc read too large",
                        self.path
                    )));
                }
                let next = (self.buf.len() * 2).min(MAX_READ);
                self.buf.resize(next, 0);
            }
            match self.read_at(filled) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(&self.buf[..filled])
    }

    pub fn text(&mut self) -> io::Result<String> {
        let bytes = self.read()?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn read_at(&mut self, offset: usize) -> io::Result<usize> {
        if !self.no_pread {
            match self.file.read_at(&mut self.buf[offset..], offset as u64) {
                Err(error) if pread_rejected(&error) => self.no_pread = true,
                other => return other,
            }
        }
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.read(&mut self.buf[offset..])
    }
}

fn pread_rejected(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ESPIPE | libc::ENOTSUP)
    )
}

/// Up to `buf.len()` bytes of `path` from its start. `Ok(buf.len())` means the file may be
/// longer than the buffer.
pub fn read_into(path: &str, buf: &mut [u8]) -> io::Result<usize> {
    let mut file = File::open(path)?;
    let mut filled = 0;
    let mut positioned = true;
    while filled < buf.len() {
        let read = if positioned {
            match file.read_at(&mut buf[filled..], filled as u64) {
                Err(error) if pread_rejected(&error) => {
                    positioned = false;
                    file.seek(SeekFrom::Start(filled as u64))?;
                    continue;
                }
                other => other,
            }
        } else {
            file.read(&mut buf[filled..])
        };
        match read {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_file_reads_its_new_contents() {
        let dir = std::env::temp_dir().join(format!("vitals-procfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stat");
        std::fs::write(&path, "cpu 1 0 0 1\n").unwrap();
        let mut file = ProcFile::open(path.to_str().unwrap()).unwrap();
        assert_eq!(file.read().unwrap(), b"cpu 1 0 0 1\n");
        let longer = "cpu 9 0 0 9\ncpu0 4 0 0 4\n".repeat(600);
        std::fs::write(&path, &longer).unwrap();
        assert_eq!(file.read().unwrap(), longer.as_bytes());
        assert_eq!(file.read().unwrap(), longer.as_bytes());
        let mut small = [0u8; 4];
        assert_eq!(read_into(path.to_str().unwrap(), &mut small).unwrap(), 4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
