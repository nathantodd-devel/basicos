//! Files and directories, on the kernel's DOS-style drive letters
//! (`A:` the ramdisk, `B:` the read-only initrd, `C:` the main disk — see
//! `kernel/src/fs`). Paths are 8.3-only FAT32: no long file names.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::errno::{Errno, Result, check};
use crate::sys::{self, nr};

pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_CREAT: usize = 0o100;
pub const O_TRUNC: usize = 0o1000;

/// Copy `s` into a NUL-terminated stack buffer for the kernel (which
/// reads paths as bounded C strings) and hand `f` a pointer to it.
pub(crate) fn with_cstr<R>(s: &str, f: impl FnOnce(*const u8) -> R) -> Result<R> {
    let mut buf = [0u8; 256];
    if s.len() >= buf.len() || s.as_bytes().contains(&0) {
        return Err(Errno::EINVAL);
    }
    buf[..s.len()].copy_from_slice(s.as_bytes());
    Ok(f(buf.as_ptr()))
}

/// An open file. Closed (and, if written to, flushed to disk) on drop.
///
/// The kernel buffers a whole file in memory from `open` until `close`,
/// so writes only reach the disk when the `File` is dropped or
/// explicitly [`close`](File::close)d.
pub struct File {
    fd: usize,
}

impl File {
    pub fn open(path: &str) -> Result<File> {
        Self::open_with(path, O_RDONLY)
    }

    /// Create (or truncate) `path` for writing.
    pub fn create(path: &str) -> Result<File> {
        Self::open_with(path, O_WRONLY | O_CREAT | O_TRUNC)
    }

    pub fn open_with(path: &str, flags: usize) -> Result<File> {
        let r = with_cstr(path, |p| unsafe { sys::syscall3(nr::OPEN, p as usize, flags, 0) })?;
        check(r).map(|fd| File { fd })
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        check(unsafe { sys::syscall3(nr::READ, self.fd, buf.as_mut_ptr() as usize, buf.len()) })
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        crate::io::write(self.fd, buf)
    }

    pub fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        crate::io::write_all(self.fd, buf)
    }

    /// Read to end of file, appending to `out`. Returns bytes read.
    pub fn read_to_end(&mut self, out: &mut Vec<u8>) -> Result<usize> {
        let mut total = 0;
        let mut chunk = [0u8; 512];
        loop {
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Ok(total);
            }
            out.extend_from_slice(&chunk[..n]);
            total += n;
        }
    }

    pub fn read_to_string(&mut self) -> Result<String> {
        let mut v = Vec::new();
        self.read_to_end(&mut v)?;
        String::from_utf8(v).map_err(|_| Errno::EINVAL)
    }

    /// `whence`: 0 = from start, 1 = from current, 2 = from end.
    pub fn seek(&mut self, offset: i64, whence: usize) -> Result<u64> {
        check(unsafe { sys::syscall3(nr::LSEEK, self.fd, offset as usize, whence) })
            .map(|p| p as u64)
    }

    /// Close now and report any error (flushing a written file to disk
    /// can fail); dropping a `File` closes it too but discards that error.
    pub fn close(self) -> Result<()> {
        let fd = self.fd;
        core::mem::forget(self);
        check(unsafe { sys::syscall1(nr::CLOSE, fd) }).map(|_| ())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { sys::syscall1(nr::CLOSE, self.fd) };
    }
}

impl fmt::Write for File {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_all(s.as_bytes()).map_err(|_| fmt::Error)
    }
}

/// Convenience: read a whole file into a `String`.
pub fn read_to_string(path: &str) -> Result<String> {
    File::open(path)?.read_to_string()
}

/// Convenience: create/truncate `path` and write `data` to it.
pub fn write(path: &str, data: &[u8]) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(data)?;
    f.close()
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// List a directory. Names come back as the kernel's 8.3 form
/// (`HELLO.ELF`), in on-disk order.
pub fn read_dir(path: &str) -> Result<Vec<DirEntry>> {
    // Directories open through the same `open` as files.
    let dir = File::open(path)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        // linux_dirent64: d_ino(8) d_off(8) d_reclen(2) d_type(1) d_name[]
        let n = check(unsafe {
            sys::syscall3(nr::GETDENTS64, dir.fd, buf.as_mut_ptr() as usize, buf.len())
        })?;
        if n == 0 {
            return Ok(out);
        }
        let mut off = 0;
        while off + 19 <= n {
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            let is_dir = buf[off + 18] == 4; // DT_DIR
            let name_bytes = &buf[off + 19..off + reclen];
            let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
            out.push(DirEntry {
                name: String::from_utf8_lossy(&name_bytes[..end]).into_owned(),
                is_dir,
            });
            if reclen == 0 {
                break;
            }
            off += reclen;
        }
    }
}
