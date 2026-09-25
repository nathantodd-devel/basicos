//! Error type for syscalls that report failure as `-errno` in RAX —
//! values match `kernel/src/syscall/mod.rs`'s `errno` module (which
//! matches Linux's numbering).

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    pub const ENOENT: Errno = Errno(2);
    pub const EIO: Errno = Errno(5);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const EAGAIN: Errno = Errno(11);
    pub const ENOMEM: Errno = Errno(12);
    pub const EFAULT: Errno = Errno(14);
    pub const ENODEV: Errno = Errno(19);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const EMFILE: Errno = Errno(24);
    pub const ENOSYS: Errno = Errno(38);

    fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            2 => "no such file or directory",
            5 => "I/O error",
            9 => "bad file descriptor",
            10 => "no child processes",
            11 => "try again",
            12 => "out of memory",
            14 => "bad address",
            19 => "no such device (drive not mounted)",
            20 => "not a directory",
            21 => "is a directory",
            22 => "invalid argument",
            24 => "too many open files",
            38 => "function not implemented",
            _ => return None,
        })
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(n) => write!(f, "{} (errno {})", n, self.0),
            None => write!(f, "errno {}", self.0),
        }
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub type Result<T> = core::result::Result<T, Errno>;

/// Turn a raw syscall return value into a `Result`: negative is `-errno`.
pub(crate) fn check(ret: isize) -> Result<usize> {
    if ret < 0 {
        Err(Errno(-ret as i32))
    } else {
        Ok(ret as usize)
    }
}
