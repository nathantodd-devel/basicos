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
pub const O_RDWR: usize = 2;
pub const O_CREAT: usize = 0o100;
pub const O_EXCL: usize = 0o200;
pub const O_TRUNC: usize = 0o1000;
pub const O_APPEND: usize = 0o2000;
pub const O_NONBLOCK: usize = 0o4000;
pub const O_DIRECTORY: usize = 0o200000;
pub const O_CLOEXEC: usize = 0o2000000;

pub const SEEK_SET: usize = 0;
pub const SEEK_CUR: usize = 1;
pub const SEEK_END: usize = 2;

/// Copy `s` into a NUL-terminated stack buffer for the kernel (which
/// reads paths as bounded C strings) and hand `f` a pointer to it.
pub(crate) fn with_cstr<R>(s: &str, f: impl FnOnce(*const u8) -> R) -> Result<R> {
    let mut buf = [0u8; 512];
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

impl File {
    /// Open for reading *and* writing (the file must exist).
    pub fn open_rw(path: &str) -> Result<File> {
        Self::open_with(path, O_RDWR)
    }

    /// Open for appending, creating the file if needed.
    pub fn append(path: &str) -> Result<File> {
        Self::open_with(path, O_WRONLY | O_CREAT | O_APPEND)
    }

    /// The raw file descriptor.
    pub fn fd(&self) -> usize {
        self.fd
    }

    /// Size and kind of the open file.
    pub fn metadata(&self) -> Result<Metadata> {
        let mut st = [0u8; 144];
        check(unsafe { sys::syscall2(nr::FSTAT, self.fd, st.as_mut_ptr() as usize) })?;
        Ok(Metadata::from_stat(&st))
    }

    /// Truncate or zero-extend to `len` bytes.
    pub fn set_len(&mut self, len: u64) -> Result<()> {
        check(unsafe { sys::syscall2(nr::FTRUNCATE, self.fd, len as usize) }).map(|_| ())
    }

    /// Write buffered changes to disk now (otherwise it happens on close).
    pub fn sync_all(&mut self) -> Result<()> {
        check(unsafe { sys::syscall1(nr::FSYNC, self.fd) }).map(|_| ())
    }

    /// Read at `offset` without moving the file position.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        check(unsafe {
            sys::syscall4(nr::PREAD64, self.fd, buf.as_mut_ptr() as usize, buf.len(), offset as usize)
        })
    }

    /// Write at `offset` without moving the file position.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        check(unsafe {
            sys::syscall4(nr::PWRITE64, self.fd, buf.as_ptr() as usize, buf.len(), offset as usize)
        })
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "File(fd {})", self.fd)
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

/// What `stat` says about a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub size: u64,
    pub is_dir: bool,
    /// The raw `st_mode` (file type bits + permissions).
    pub mode: u32,
    /// Hard-link count (1 where the filesystem doesn't say).
    pub nlink: u64,
    /// Seconds since the epoch; 0 where the filesystem doesn't record it.
    pub mtime: u64,
    pub uid: u32,
    pub gid: u32,
}

impl Metadata {
    pub(crate) fn from_stat(st: &[u8; 144]) -> Metadata {
        let mode = u32::from_le_bytes([st[24], st[25], st[26], st[27]]);
        let size = u64::from_le_bytes(st[48..56].try_into().unwrap());
        Metadata {
            size,
            is_dir: mode & 0o170000 == 0o040000,
            mode,
            nlink: u64::from_le_bytes(st[16..24].try_into().unwrap()),
            mtime: u64::from_le_bytes(st[88..96].try_into().unwrap()),
            uid: u32::from_le_bytes(st[28..32].try_into().unwrap()),
            gid: u32::from_le_bytes(st[32..36].try_into().unwrap()),
        }
    }

    pub fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }

    /// Permission bits (`0o644`), without the file type.
    pub fn permissions(&self) -> u32 {
        self.mode & 0o7777
    }
}

/// `stat(path)`.
pub fn metadata(path: &str) -> Result<Metadata> {
    let mut st = [0u8; 144];
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::STAT, p as usize, st.as_mut_ptr() as usize) })?)?;
    Ok(Metadata::from_stat(&st))
}

pub fn exists(path: &str) -> bool {
    metadata(path).is_ok()
}

pub fn create_dir(path: &str) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::MKDIR, p as usize, 0o755) })?).map(|_| ())
}

pub fn remove_file(path: &str) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall1(nr::UNLINK, p as usize) })?).map(|_| ())
}

/// Remove an empty directory.
pub fn remove_dir(path: &str) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall1(nr::RMDIR, p as usize) })?).map(|_| ())
}

/// Move/rename a file or directory (within one drive).
pub fn rename(from: &str, to: &str) -> Result<()> {
    let r = with_cstr(from, |f| with_cstr(to, |t| unsafe { sys::syscall2(nr::RENAME, f as usize, t as usize) }))??;
    check(r).map(|_| ())
}

/// Truncate/extend the file at `path` to `len` bytes.
pub fn truncate(path: &str, len: u64) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::TRUNCATE, p as usize, len as usize) })?).map(|_| ())
}

/// Change the working directory; relative paths resolve against it.
pub fn set_current_dir(path: &str) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall1(nr::CHDIR, p as usize) })?).map(|_| ())
}

/// The working directory, DOS-style (`C:/some/dir`).
pub fn current_dir() -> Result<String> {
    let mut buf = [0u8; 512];
    let n = check(unsafe { sys::syscall2(nr::GETCWD, buf.as_mut_ptr() as usize, buf.len()) })?;
    // The kernel returns the length including the NUL.
    String::from_utf8(buf[..n.saturating_sub(1)].to_vec()).map_err(|_| Errno::EINVAL)
}

/// Like [`metadata`] but describes a symlink itself instead of following it.
pub fn symlink_metadata(path: &str) -> Result<Metadata> {
    let mut st = [0u8; 144];
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::LSTAT, p as usize, st.as_mut_ptr() as usize) })?)?;
    Ok(Metadata::from_stat(&st))
}

/// Create a symbolic link at `link` pointing to `target` (ext4 volumes).
pub fn symlink(target: &str, link: &str) -> Result<()> {
    let r = with_cstr(target, |t| with_cstr(link, |l| unsafe { sys::syscall2(nr::SYMLINK, t as usize, l as usize) }))??;
    check(r).map(|_| ())
}

/// Where the symlink at `path` points.
pub fn read_link(path: &str) -> Result<String> {
    let mut buf = [0u8; 1024];
    let n = check(with_cstr(path, |p| unsafe {
        sys::syscall3(nr::READLINK, p as usize, buf.as_mut_ptr() as usize, buf.len())
    })?)?;
    String::from_utf8(buf[..n].to_vec()).map_err(|_| Errno::EINVAL)
}

/// Make `new` another name for the file `existing` (ext4 volumes).
pub fn hard_link(existing: &str, new: &str) -> Result<()> {
    let r = with_cstr(existing, |o| with_cstr(new, |n| unsafe { sys::syscall2(nr::LINK, o as usize, n as usize) }))??;
    check(r).map(|_| ())
}

/// Set permission bits (`0o644`).
pub fn set_permissions(path: &str, mode: u32) -> Result<()> {
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::CHMOD, p as usize, mode as usize) })?).map(|_| ())
}

/// Space on the volume holding `path`.
#[derive(Debug, Clone, Copy)]
pub struct FsStats {
    /// The filesystem's type magic (`0xEF53` ext4, `0x4d44` FAT).
    pub magic: u64,
    pub block_size: u64,
    pub blocks: u64,
    pub free_blocks: u64,
}

pub fn statfs(path: &str) -> Result<FsStats> {
    let mut b = [0u8; 120];
    check(with_cstr(path, |p| unsafe { sys::syscall2(nr::STATFS, p as usize, b.as_mut_ptr() as usize) })?)?;
    let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    Ok(FsStats { magic: q(0), block_size: q(8), blocks: q(16), free_blocks: q(24) })
}

/// Flush every filesystem's cached data to disk.
pub fn sync() {
    unsafe { sys::syscall0(nr::SYNC) };
}
