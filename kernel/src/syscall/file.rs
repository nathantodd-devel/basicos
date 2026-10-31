//! Path-based and file-descriptor file syscalls: open/stat/mkdir/unlink/
//! rename/chdir/..., on top of the three FAT32 volumes (`A:` ramdisk,
//! `B:` initrd, `C:` disk).
//!
//! Paths may be absolute (`/x`, `C:/x`) or relative to the process's
//! current directory (`chdir`), and `.`/`..` are resolved. The
//! filesystem is 8.3 with no long names, so anything longer fails with
//! `ENAMETOOLONG`.

use super::fd::{self, Backing, Dir, File, Handle, Object};
use super::io::{put_stat, sys_fstat, sys_read, sys_write};
use super::{errno, open_flags as of, uaccess, whence};
use crate::fs::{self, Drive, FileSystem, FsError};
use alloc::string::String;
use alloc::vec::Vec;
use basicos_fs::path;

const AT_FDCWD: i64 = -100;
const AT_REMOVEDIR: u64 = 0x200;

/// A resolved path: which drive, and the normalized drive-relative path.
pub struct Resolved {
    pub drive: Drive,
    pub rel: String,
}

impl Resolved {
    /// `C:/dir/file` — the form `chdir`/`getcwd` keep.
    fn dos(&self) -> String {
        alloc::format!("{}:/{}", self.drive.letter(), self.rel)
    }
}

fn split_dos(cwd: &str) -> Resolved {
    let (drive, rel) = fs::resolve_path(cwd);
    Resolved { drive, rel: path::normalize("", rel) }
}

/// Resolve `p` against `base` (a DOS-form directory).
fn resolve_in(base: &str, p: &str) -> Resolved {
    let explicit_drive = p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic();
    if explicit_drive || p.starts_with('/') {
        let (drive, rest) = fs::resolve_path(p);
        Resolved { drive, rel: path::normalize("", rest) }
    } else {
        let cwd = split_dos(base);
        Resolved { drive: cwd.drive, rel: path::normalize(&cwd.rel, p) }
    }
}

pub fn resolve(p: &str) -> Resolved {
    resolve_in(&fd::cwd(), p)
}

/// The base directory for `*at()` calls: the cwd for `AT_FDCWD`, else the
/// directory the fd refers to.
fn base_for(dirfd: i64) -> Result<String, i64> {
    if dirfd == AT_FDCWD {
        return Ok(fd::cwd());
    }
    let h = fd::get(dirfd as u64)?;
    let d = h.lock();
    match &d.obj {
        Object::Dir(dir) => Ok(alloc::format!("{}:/{}", dir.drive.letter(), dir.path)),
        _ => Err(errno::ENOTDIR),
    }
}

fn resolve_at(dirfd: i64, p: &str) -> Result<Resolved, i64> {
    Ok(resolve_in(&base_for(dirfd)?, p))
}

pub(super) fn fs_err(e: FsError) -> i64 {
    match e {
        FsError::NotFound => errno::ENOENT,
        FsError::NotAFile => errno::EISDIR,
        FsError::NotADirectory => errno::ENOTDIR,
        FsError::NoSpace => errno::ENOSPC,
        FsError::NameTooLong => errno::ENAMETOOLONG,
        FsError::AlreadyExists => errno::EEXIST,
        FsError::NotEmpty => errno::ENOTEMPTY,
        FsError::Invalid => errno::EINVAL,
        FsError::ReadOnly => errno::EROFS,
        FsError::Unsupported => errno::EOPNOTSUPP,
        FsError::TooManyLinks => errno::ELOOP,
        FsError::TooBig => errno::EFBIG,
        FsError::Io | FsError::NotFat32 | FsError::Corrupt => errno::EIO,
    }
}

/// Run `f` on the volume, mapping "not mounted" to `ENODEV`.
fn on_drive<R>(drive: Drive, f: impl FnOnce(&mut dyn FileSystem) -> Result<R, FsError>) -> Result<R, i64> {
    match fs::with_drive(drive, f) {
        None => Err(errno::ENODEV),
        Some(r) => r.map_err(fs_err),
    }
}

/// Whether writes to this volume must be refused: the initrd (its
/// media is read-only), or a volume whose driver says so (ext4 with an
/// unsupported feature, or on a read-only device).
pub(super) fn volume_read_only(drive: Drive) -> bool {
    drive == Drive::Initrd || fs::with_drive(drive, |f| f.read_only()).unwrap_or(true)
}

fn check_writable(drive: Drive) -> Result<(), i64> {
    if volume_read_only(drive) {
        Err(errno::EROFS)
    } else {
        Ok(())
    }
}

// --- open ---------------------------------------------------------------

fn device_for(r: &Resolved) -> Option<Object> {
    if r.drive != Drive::Disk {
        return None;
    }
    match r.rel.as_str() {
        "dev/null" => Some(Object::Null),
        "dev/zero" => Some(Object::Zero),
        "dev/random" | "dev/urandom" => Some(Object::Random),
        _ => None,
    }
}

/// Flags that stay on the description after open (the rest — `O_CREAT`,
/// `O_TRUNC`, ... — only affect the open itself).
const KEEP: u64 = of::O_ACCMODE | of::O_APPEND | of::O_NONBLOCK;

fn open_resolved(r: Resolved, flags: u64) -> Result<Handle, i64> {
    let acc = flags & of::O_ACCMODE;
    let writing = acc != of::O_RDONLY;

    if let Some(dev) = device_for(&r) {
        return Ok(fd::new_handle(dev, flags & KEEP));
    }
    if r.drive == Drive::Disk {
        if let Some(name) = r.rel.strip_prefix("proc/") {
            if writing {
                return Err(errno::EROFS);
            }
            let data = super::procfs::read(name).ok_or(errno::ENOENT)?;
            let file = fd::ProcFile { data, pos: 0 };
            return Ok(fd::new_handle(Object::Proc(file), flags & KEEP));
        }
    }
    if r.drive == Drive::Disk && matches!(r.rel.as_str(), "dev/tty" | "dev/console") {
        let obj = if writing { Object::ConsoleOut } else { Object::ConsoleIn };
        return Ok(fd::new_handle(obj, flags & KEEP));
    }
    if writing || flags & (of::O_CREAT | of::O_TRUNC) != 0 {
        check_writable(r.drive)?;
    }

    let creating = flags & of::O_CREAT != 0;
    let truncating = flags & of::O_TRUNC != 0;

    // `None` = doesn't exist.
    let existing = if r.rel.is_empty() {
        Some(fs::DirEntry { is_dir: true, ..Default::default() })
    } else {
        match on_drive(r.drive, |v| v.find(&r.rel)) {
            Ok(e) => Some(e),
            Err(e) if e == errno::ENOENT => None,
            Err(e) => return Err(e),
        }
    };

    match existing {
        Some(entry) if entry.is_dir => {
            if writing {
                return Err(errno::EISDIR);
            }
            let entries = on_drive(r.drive, |v| v.read_dir_at(&r.rel))?;
            Ok(fd::new_handle(
                Object::Dir(Dir { drive: r.drive, path: r.rel, entries, pos: 0 }),
                flags & KEEP,
            ))
        }
        Some(_) if creating && flags & of::O_EXCL != 0 => Err(errno::EEXIST),
        Some(_) if flags & of::O_DIRECTORY != 0 => Err(errno::ENOTDIR),
        Some(entry) => {
            let truncate_now = truncating && writing;
            // Volumes that can address a file directly (ext4) get a
            // streaming handle; the rest (FAT32) are buffered whole.
            let stream = on_drive(r.drive, |v| v.open_inode(&r.rel, false, truncate_now))?;
            let backing = match stream {
                Some(ino) => Backing::Stream { ino },
                None => {
                    let data = if truncate_now { Vec::new() } else { on_drive(r.drive, |v| v.read_file(&entry))? };
                    Backing::Mem { data, dirty: truncate_now }
                }
            };
            Ok(fd::new_handle(
                Object::File(File { drive: r.drive, path: r.rel, pos: 0, backing }),
                flags & KEEP,
            ))
        }
        None if creating => {
            // Create it on disk right away, so it exists (for `stat`,
            // `ls`, another open) even before the first flush.
            let stream = on_drive(r.drive, |v| v.open_inode(&r.rel, true, false))?;
            let backing = match stream {
                Some(ino) => Backing::Stream { ino },
                None => {
                    on_drive(r.drive, |v| v.write_file(&r.rel, &[]))?;
                    Backing::Mem { data: Vec::new(), dirty: false }
                }
            };
            Ok(fd::new_handle(
                Object::File(File { drive: r.drive, path: r.rel, pos: 0, backing }),
                flags & KEEP,
            ))
        }
        None => Err(errno::ENOENT),
    }
}

fn do_open(dirfd: i64, path_ptr: u64, flags: u64) -> i64 {
    let path = match uaccess::cstring(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = match resolve_at(dirfd, &path) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match open_resolved(r, flags) {
        Ok(h) => match fd::install(h, flags & of::O_CLOEXEC != 0) {
            Ok(n) => n as i64,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

pub fn sys_open(path: u64, flags: u64, _mode: u64) -> i64 {
    do_open(AT_FDCWD, path, flags)
}

pub fn sys_openat(dirfd: u64, path: u64, flags: u64, _mode: u64) -> i64 {
    do_open(dirfd as i64, path, flags)
}

// --- read/write variants -------------------------------------------------

/// pread64/pwrite64: transfer at `offset` without moving the file position.
fn positional(fd_: u64, buf: u64, count: u64, offset: i64, write: bool) -> i64 {
    if offset < 0 {
        return errno::EINVAL;
    }
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let mut d = h.lock();
    let acc = d.flags & of::O_ACCMODE;
    let Object::File(f) = &mut d.obj else {
        return match &d.obj {
            Object::Dir(_) => errno::EISDIR,
            _ => errno::ESPIPE,
        };
    };
    let saved = f.pos;
    f.pos = offset as usize;
    drop(d);
    // Reuse the ordinary paths (they take the lock themselves), then
    // restore the position.
    let n = if write {
        if acc == of::O_RDONLY { errno::EBADF } else { sys_write(fd_, buf, count) }
    } else {
        sys_read(fd_, buf, count)
    };
    if let Object::File(f) = &mut h.lock().obj {
        f.pos = saved;
    }
    n
}

pub fn sys_pread64(fd_: u64, buf: u64, count: u64, offset: u64) -> i64 {
    positional(fd_, buf, count, offset as i64, false)
}

pub fn sys_pwrite64(fd_: u64, buf: u64, count: u64, offset: u64) -> i64 {
    positional(fd_, buf, count, offset as i64, true)
}

// --- lseek / getdents ----------------------------------------------------

pub fn sys_lseek(fd_: u64, offset: u64, wh: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let offset = offset as i64;
    let mut d = h.lock();
    match &mut d.obj {
        Object::File(f) => {
            let base = match wh {
                whence::SEEK_SET => 0,
                whence::SEEK_CUR => f.pos as i64,
                whence::SEEK_END => match f.size() {
                    Ok(n) => n as i64,
                    Err(e) => return e,
                },
                _ => return errno::EINVAL,
            };
            match base.checked_add(offset) {
                Some(p) if p >= 0 => {
                    f.pos = p as usize;
                    p
                }
                _ => errno::EINVAL,
            }
        }
        Object::Dir(dir) => {
            // Only rewinding is meaningful.
            if wh == whence::SEEK_SET && offset == 0 {
                dir.pos = 0;
                0
            } else {
                errno::EINVAL
            }
        }
        Object::Null | Object::Zero | Object::Random => 0,
        _ => errno::ESPIPE,
    }
}

/// getdents64(fd, dirp, count) — packed `linux_dirent64` records, the
/// ABI glibc's `readdir()` walks:
/// `{ u64 d_ino; i64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }`,
/// each record padded to 8 bytes. Returns the bytes written, 0 at the end.
pub fn sys_getdents64(fd_: u64, dirp: u64, count: u64) -> i64 {
    const HEADER_LEN: usize = 19;
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;
    const DT_LNK: u8 = 10;

    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let out = match uaccess::slice_mut(dirp, count as usize) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut d = h.lock();
    let Object::Dir(dir) = &mut d.obj else {
        return errno::ENOTDIR;
    };
    let mut written = 0usize;
    while dir.pos < dir.entries.len() {
        let entry = &dir.entries[dir.pos];
        let name = entry.name.as_bytes();
        let reclen = (HEADER_LEN + name.len() + 1 + 7) & !7;
        if written + reclen > out.len() {
            break;
        }
        let rec = &mut out[written..written + reclen];
        rec.fill(0);
        let ino = if entry.first_cluster != 0 { entry.first_cluster as u64 } else { dir.pos as u64 + 1 };
        rec[0..8].copy_from_slice(&ino.to_le_bytes());
        rec[8..16].copy_from_slice(&(dir.pos as u64 + 1).to_le_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = if entry.attr.is_symlink { DT_LNK } else if entry.is_dir { DT_DIR } else { DT_REG };
        rec[HEADER_LEN..HEADER_LEN + name.len()].copy_from_slice(name);
        written += reclen;
        dir.pos += 1;
    }
    written as i64
}

// --- stat ----------------------------------------------------------------

fn entry_mode(e: &fs::DirEntry) -> u32 {
    let kind = if e.attr.is_symlink {
        0o120000
    } else if e.is_dir {
        0o040000
    } else {
        0o100000
    };
    // Formats without permissions (FAT32) get the traditional defaults.
    let perm = if e.attr.perm != 0 || e.attr.nlink != 0 { e.attr.perm } else if e.is_dir { 0o755 } else { 0o644 };
    kind | perm
}

/// Fill a stat record from a directory entry.
pub(super) fn apply_entry(st: &mut super::io::StatInfo, e: &fs::DirEntry) {
    st.mode = entry_mode(e);
    st.size = e.size;
    st.ino = e.first_cluster as u64;
    st.nlink = e.attr.nlink as u64;
    st.uid = e.attr.uid;
    st.gid = e.attr.gid;
    st.atime = e.attr.atime;
    st.mtime = e.attr.mtime;
    st.ctime = e.attr.ctime;
}

fn stat_path(r: &Resolved, buf: u64, follow: bool) -> i64 {
    if let Some(dev) = device_for(r) {
        let _ = dev;
        return put_stat(buf, 0o020666, 0, 0);
    }
    if r.rel.is_empty() {
        return put_stat(buf, 0o040755, 0, 1);
    }
    let found = on_drive(r.drive, |v| if follow { v.find(&r.rel) } else { v.find_nofollow(&r.rel) });
    match found {
        Ok(e) => {
            let mut st = super::io::StatInfo::default();
            apply_entry(&mut st, &e);
            super::io::put_stat_info(buf, &st)
        }
        Err(e) => e,
    }
}

pub fn sys_stat(path: u64, buf: u64) -> i64 {
    match uaccess::cstring(path) {
        Ok(p) => stat_path(&resolve(&p), buf, true),
        Err(e) => e,
    }
}

/// lstat: like stat, but a symlink is described, not followed.
pub fn sys_lstat(path: u64, buf: u64) -> i64 {
    match uaccess::cstring(path) {
        Ok(p) => stat_path(&resolve(&p), buf, false),
        Err(e) => e,
    }
}

pub fn sys_newfstatat(dirfd: u64, path: u64, buf: u64, flags: u64) -> i64 {
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if p.is_empty() {
        return sys_fstat(dirfd, buf); // AT_EMPTY_PATH-style
    }
    match resolve_at(dirfd as i64, &p) {
        Ok(r) => stat_path(&r, buf, flags & AT_SYMLINK_NOFOLLOW == 0),
        Err(e) => e,
    }
}

pub fn sys_access(path: u64, mode: u64) -> i64 {
    const W_OK: u64 = 2;
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    if device_for(&r).is_none() && !r.rel.is_empty() {
        if let Err(e) = on_drive(r.drive, |v| v.find(&r.rel)) {
            return e;
        }
    }
    if mode & W_OK != 0 && r.drive == Drive::Initrd {
        return errno::EROFS;
    }
    0
}

// --- directory operations ---------------------------------------------

pub fn sys_mkdir(path: u64, _mode: u64) -> i64 {
    mkdir_at(AT_FDCWD, path)
}

pub fn sys_mkdirat(dirfd: u64, path: u64, _mode: u64) -> i64 {
    mkdir_at(dirfd as i64, path)
}

fn mkdir_at(dirfd: i64, path_ptr: u64) -> i64 {
    let p = match uaccess::cstring(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = match resolve_at(dirfd, &p) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if let Err(e) = check_writable(r.drive) {
        return e;
    }
    match on_drive(r.drive, |v| v.create_dir(&r.rel)) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_rmdir(path: u64) -> i64 {
    unlink_at(AT_FDCWD, path, AT_REMOVEDIR)
}

pub fn sys_unlink(path: u64) -> i64 {
    unlink_at(AT_FDCWD, path, 0)
}

pub fn sys_unlinkat(dirfd: u64, path: u64, flags: u64) -> i64 {
    unlink_at(dirfd as i64, path, flags)
}

fn unlink_at(dirfd: i64, path_ptr: u64, flags: u64) -> i64 {
    let p = match uaccess::cstring(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = match resolve_at(dirfd, &p) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if let Err(e) = check_writable(r.drive) {
        return e;
    }
    if r.rel.is_empty() {
        return errno::EBUSY; // the root
    }
    // Refuse to remove the directory we're standing in.
    if flags & AT_REMOVEDIR != 0 && split_dos(&fd::cwd()).rel == r.rel && split_dos(&fd::cwd()).drive == r.drive {
        return errno::EBUSY;
    }
    let result = if flags & AT_REMOVEDIR != 0 {
        on_drive(r.drive, |v| v.remove_dir(&r.rel))
    } else {
        // unlink(2) on a directory is EISDIR, not "not a file".
        on_drive(r.drive, |v| v.delete_file(&r.rel))
    };
    match result {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_rename(old: u64, new: u64) -> i64 {
    let (o, n) = match (uaccess::cstring(old), uaccess::cstring(new)) {
        (Ok(o), Ok(n)) => (o, n),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let (ro, rn) = (resolve(&o), resolve(&n));
    if ro.drive != rn.drive {
        return errno::EXDEV;
    }
    if let Err(e) = check_writable(ro.drive) {
        return e;
    }
    match on_drive(ro.drive, |v| v.rename(&ro.rel, &rn.rel)) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_chdir(path: u64) -> i64 {
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    if !r.rel.is_empty() {
        match on_drive(r.drive, |v| v.find(&r.rel)) {
            Ok(e) if e.is_dir => {}
            Ok(_) => return errno::ENOTDIR,
            Err(e) => return e,
        }
    } else if fs::with_drive(r.drive, |_| ()).is_none() {
        return errno::ENODEV;
    }
    fd::set_cwd(r.dos());
    0
}

pub fn sys_getcwd(buf: u64, size: u64) -> i64 {
    let cwd = fd::cwd();
    let bytes = cwd.as_bytes();
    if (size as usize) < bytes.len() + 1 {
        return errno::ERANGE;
    }
    let out = match uaccess::slice_mut(buf, bytes.len() + 1) {
        Ok(o) => o,
        Err(e) => return e,
    };
    out[..bytes.len()].copy_from_slice(bytes);
    out[bytes.len()] = 0;
    (bytes.len() + 1) as i64
}

// --- size changes / sync -----------------------------------------------

pub fn sys_ftruncate(fd_: u64, len: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let mut d = h.lock();
    if d.flags & of::O_ACCMODE == of::O_RDONLY {
        return errno::EINVAL;
    }
    match &mut d.obj {
        Object::File(f) => match f.truncate(len) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Object::Dir(_) => errno::EISDIR,
        _ => errno::EINVAL,
    }
}

pub fn sys_truncate(path: u64, len: u64) -> i64 {
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    if let Err(e) = check_writable(r.drive) {
        return e;
    }
    let result = on_drive(r.drive, |v| {
        let entry = v.find(&r.rel)?;
        let mut data = v.read_file(&entry)?;
        data.resize(len as usize, 0);
        v.write_file(&r.rel, &data)
    });
    match result {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_fsync(fd_: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let mut d = h.lock();
    match &mut d.obj {
        Object::File(f) => match f.flush() {
            Ok(()) => 0,
            Err(e) => e,
        },
        Object::PipeRead(_) | Object::PipeWrite(_) => errno::EINVAL,
        _ => 0,
    }
}

// --- fcntl / ioctl / dup / pipe ----------------------------------------

// --- symlinks, links, permissions, statfs, sync ---------------------------

pub fn sys_readlink(path: u64, buf: u64, size: u64) -> i64 {
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    let target = match on_drive(r.drive, |v| v.read_link(&r.rel)) {
        Ok(t) => t,
        Err(e) if e == errno::EINVAL => return errno::EINVAL, // not a symlink
        Err(e) => return e,
    };
    let n = target.len().min(size as usize);
    match uaccess::slice_mut(buf, n) {
        Ok(out) => {
            out.copy_from_slice(&target.as_bytes()[..n]);
            n as i64
        }
        Err(e) => e,
    }
}

pub fn sys_symlink(target: u64, linkpath: u64) -> i64 {
    let (t, l) = match (uaccess::cstring(target), uaccess::cstring(linkpath)) {
        (Ok(t), Ok(l)) => (t, l),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let r = resolve(&l);
    if let Err(e) = check_writable(r.drive) {
        return e;
    }
    on_drive(r.drive, |v| v.symlink(&t, &r.rel)).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_link(old: u64, new: u64) -> i64 {
    let (o, n) = match (uaccess::cstring(old), uaccess::cstring(new)) {
        (Ok(o), Ok(n)) => (o, n),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let (ro, rn) = (resolve(&o), resolve(&n));
    if ro.drive != rn.drive {
        return errno::EXDEV;
    }
    if let Err(e) = check_writable(ro.drive) {
        return e;
    }
    on_drive(ro.drive, |v| v.hard_link(&ro.rel, &rn.rel)).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_chmod(path: u64, mode: u64) -> i64 {
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    if let Err(e) = check_writable(r.drive) {
        return e;
    }
    on_drive(r.drive, |v| v.set_perm(&r.rel, mode as u32 & 0o7777)).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_fchmod(fd_: u64, mode: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let Some((drive, path)) = fd::file_location(&h) else { return errno::EINVAL };
    if let Err(e) = check_writable(drive) {
        return e;
    }
    on_drive(drive, |v| v.set_perm(&path, mode as u32 & 0o7777)).map(|_| 0).unwrap_or_else(|e| e)
}

/// statfs(path, buf): struct statfs, 120 bytes.
pub fn sys_statfs(path: u64, buf: u64) -> i64 {
    let p = match uaccess::cstring(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = resolve(&p);
    put_statfs(r.drive, buf)
}

pub fn sys_fstatfs(fd_: u64, buf: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    match fd::file_location(&h).map(|(d, _)| d).or_else(|| fd::dir_drive(&h)) {
        Some(d) => put_statfs(d, buf),
        None => errno::EINVAL,
    }
}

fn put_statfs(drive: Drive, buf: u64) -> i64 {
    let Some((ty, total, free)) = fs::with_drive(drive, |v| {
        let (t, f) = v.space();
        let magic = if v.fs_type() == "ext4" { 0xEF53u64 } else { 0x4d44 };
        (magic, t, f)
    }) else {
        return errno::ENODEV;
    };
    const BS: u64 = 4096;
    let mut b = [0u8; 120];
    let mut set = |off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    set(0, ty);
    set(8, BS);
    set(16, total / BS);
    set(24, free / BS);
    set(32, free / BS);
    set(64, 255); // f_namelen
    set(72, BS); // f_frsize
    match uaccess::slice_mut(buf, 120) {
        Ok(out) => {
            out.copy_from_slice(&b);
            0
        }
        Err(e) => e,
    }
}

/// sync(): write every volume's cached state and buffered files to disk.
pub fn sys_sync() -> i64 {
    fs::sync_all();
    0
}
