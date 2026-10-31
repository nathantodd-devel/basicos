//! File-descriptor syscalls that don't touch the filesystem: read/write
//! (and the vectored forms), close, dup*, pipe, fcntl, ioctl, fstat.
//! They work on whatever an fd points at — console, pipe, socket, or a
//! file — via [`super::fd`].

use super::{errno, open_flags as of, uaccess};
use super::fd::{self, Object};

pub fn sys_close(fd_: u64) -> i64 {
    fd::close(fd_)
}

pub fn sys_read(fd_: u64, buf: u64, count: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let out = match uaccess::slice_mut(buf, count as usize) {
        Ok(s) => s,
        Err(e) => return e,
    };
    fd::read(&h, out)
}

pub fn sys_write(fd_: u64, buf: u64, count: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let data = match uaccess::slice(buf, count as usize) {
        Ok(s) => s,
        Err(e) => return e,
    };
    fd::write(&h, data)
}

#[derive(Clone, Copy)]
#[repr(C)]
struct IoVec {
    base: u64,
    len: u64,
}

fn vectored(fd_: u64, iov: u64, cnt: u64, write: bool) -> i64 {
    if cnt > 1024 {
        return errno::EINVAL;
    }
    let mut total = 0i64;
    for i in 0..cnt {
        let v: IoVec = match uaccess::get(iov + i * 16) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if v.len == 0 {
            continue;
        }
        let n = if write { sys_write(fd_, v.base, v.len) } else { sys_read(fd_, v.base, v.len) };
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as u64) < v.len {
            break; // short transfer
        }
    }
    total
}

pub fn sys_readv(fd_: u64, iov: u64, cnt: u64) -> i64 {
    vectored(fd_, iov, cnt, false)
}

pub fn sys_writev(fd_: u64, iov: u64, cnt: u64) -> i64 {
    vectored(fd_, iov, cnt, true)
}

/// Write a Linux x86_64 `struct stat` (144 bytes) to user memory.
/// What `stat` reports; unknown fields are left zero (or defaulted).
#[derive(Default, Clone, Copy)]
pub(super) struct StatInfo {
    pub mode: u32,
    pub size: u64,
    pub ino: u64,
    /// 0 = unknown (reported as 1).
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

pub(super) fn put_stat(ptr: u64, mode: u32, size: u64, ino: u64) -> i64 {
    put_stat_info(ptr, &StatInfo { mode, size, ino, ..Default::default() })
}

/// Write a Linux x86_64 `struct stat` (144 bytes) to user memory.
pub(super) fn put_stat_info(ptr: u64, st: &StatInfo) -> i64 {
    let mut b = [0u8; 144];
    let mut set = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
    set(0, &1u64.to_le_bytes()); // st_dev
    set(8, &st.ino.to_le_bytes());
    set(16, &st.nlink.max(1).to_le_bytes());
    set(24, &st.mode.to_le_bytes());
    set(28, &st.uid.to_le_bytes());
    set(32, &st.gid.to_le_bytes());
    set(48, &st.size.to_le_bytes());
    set(56, &4096u64.to_le_bytes()); // st_blksize
    set(64, &st.size.div_ceil(512).to_le_bytes()); // st_blocks
    set(72, &st.atime.to_le_bytes()); // st_atim.tv_sec
    set(88, &st.mtime.to_le_bytes());
    set(104, &st.ctime.to_le_bytes());
    match uaccess::slice_mut(ptr, 144) {
        Ok(out) => {
            out.copy_from_slice(&b);
            0
        }
        Err(e) => e,
    }
}

pub fn sys_fstat(fd_: u64, buf: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let (kind, size) = fd::stat_kind(&h);
    let perm = if kind == 0o040000 { 0o755 } else { 0o644 };
    let mut st = StatInfo { mode: kind | perm, size, ..Default::default() };
    // Files know their volume and path: ask it for the real attributes.
    #[cfg(feature = "storage")]
    if let Some((drive, path)) = fd::file_location(&h) {
        if let Some(Ok(e)) = crate::fs::with_drive(drive, |v| v.find(&path)) {
            super::file::apply_entry(&mut st, &e);
            st.size = size; // the open description's view (unflushed writes)
        }
    }
    put_stat_info(buf, &st)
}

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const FD_CLOEXEC: u64 = 1;

pub fn sys_fcntl(fd_: u64, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => fd::dup(fd_, None, arg as usize, false),
        F_DUPFD_CLOEXEC => fd::dup(fd_, None, arg as usize, true),
        F_GETFD => match fd::get_cloexec(fd_) {
            Ok(c) => c as i64 * FD_CLOEXEC as i64,
            Err(e) => e,
        },
        F_SETFD => match fd::set_cloexec(fd_, arg & FD_CLOEXEC != 0) {
            Ok(()) => 0,
            Err(e) => e,
        },
        F_GETFL => match fd::get(fd_) {
            Ok(h) => h.lock().flags as i64,
            Err(e) => e,
        },
        F_SETFL => match fd::get(fd_) {
            Ok(h) => {
                // Only O_APPEND and O_NONBLOCK can change after open.
                let mut d = h.lock();
                let mask = of::O_APPEND | of::O_NONBLOCK;
                d.flags = (d.flags & !mask) | (arg & mask);
                0
            }
            Err(e) => e,
        },
        _ => errno::EINVAL,
    }
}

const TCGETS: u64 = 0x5401;
const TIOCGWINSZ: u64 = 0x5413;
const FIONREAD: u64 = 0x541B;

pub fn sys_ioctl(fd_: u64, req: u64, arg: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let is_tty = matches!(h.lock().obj, Object::ConsoleIn | Object::ConsoleOut);
    match req {
        TCGETS if is_tty => {
            // A plausible cooked-mode termios: ICRNL | OPOST,ONLCR |
            // CS8,CREAD | ISIG,ICANON,ECHO. (Read-only: there's nothing
            // to actually configure, so TCSETS* isn't supported.)
            let mut t = [0u8; 36];
            t[0..4].copy_from_slice(&0x100u32.to_le_bytes());
            t[4..8].copy_from_slice(&0x5u32.to_le_bytes());
            t[8..12].copy_from_slice(&0xB0u32.to_le_bytes());
            t[12..16].copy_from_slice(&0x8Bu32.to_le_bytes());
            match uaccess::slice_mut(arg, t.len()) {
                Ok(o) => {
                    o.copy_from_slice(&t);
                    0
                }
                Err(e) => e,
            }
        }
        TIOCGWINSZ if is_tty => {
            let mut w = [0u8; 8];
            w[0..2].copy_from_slice(&25u16.to_le_bytes());
            w[2..4].copy_from_slice(&80u16.to_le_bytes());
            match uaccess::slice_mut(arg, w.len()) {
                Ok(o) => {
                    o.copy_from_slice(&w);
                    0
                }
                Err(e) => e,
            }
        }
        FIONREAD => match uaccess::put(arg, fd::bytes_available(&h) as u32) {
            Ok(()) => 0,
            Err(e) => e,
        },
        _ => errno::ENOTTY,
    }
}

pub fn sys_dup(fd_: u64) -> i64 {
    fd::dup(fd_, None, 0, false)
}

pub fn sys_dup2(old: u64, new: u64) -> i64 {
    if old == new {
        return match fd::get(old) {
            Ok(_) => new as i64,
            Err(e) => e,
        };
    }
    fd::dup(old, Some(new), 0, false)
}

pub fn sys_dup3(old: u64, new: u64, flags: u64) -> i64 {
    if old == new {
        return errno::EINVAL;
    }
    fd::dup(old, Some(new), 0, flags & of::O_CLOEXEC != 0)
}

pub fn sys_pipe2(fds: u64, flags: u64) -> i64 {
    match fd::make_pipe(flags) {
        Ok((r, w)) => match uaccess::put(fds, [r as i32, w as i32]) {
            Ok(()) => 0,
            Err(e) => {
                fd::close(r);
                fd::close(w);
                e
            }
        },
        Err(e) => e,
    }
}

pub fn sys_pipe(fds: u64) -> i64 {
    sys_pipe2(fds, 0)
}

pub fn sys_umask(mask: u64) -> i64 {
    fd::umask(mask as u32) as i64
}
