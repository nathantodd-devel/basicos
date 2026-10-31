//! Error type for syscalls that report failure as `-errno` in RAX —
//! values match `kernel/src/syscall/mod.rs`'s `errno` module (which
//! matches Linux's numbering).

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const ESRCH: Errno = Errno(3);
    pub const EIO: Errno = Errno(5);
    pub const E2BIG: Errno = Errno(7);
    pub const EBADF: Errno = Errno(9);
    pub const ECHILD: Errno = Errno(10);
    pub const EAGAIN: Errno = Errno(11);
    pub const ENOMEM: Errno = Errno(12);
    pub const EFAULT: Errno = Errno(14);
    pub const EBUSY: Errno = Errno(16);
    pub const EEXIST: Errno = Errno(17);
    pub const EXDEV: Errno = Errno(18);
    pub const ENODEV: Errno = Errno(19);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const EMFILE: Errno = Errno(24);
    pub const ENOTTY: Errno = Errno(25);
    pub const EFBIG: Errno = Errno(27);
    pub const ENOSPC: Errno = Errno(28);
    pub const ESPIPE: Errno = Errno(29);
    pub const EROFS: Errno = Errno(30);
    pub const EPIPE: Errno = Errno(32);
    pub const ERANGE: Errno = Errno(34);
    pub const ENAMETOOLONG: Errno = Errno(36);
    pub const ENOSYS: Errno = Errno(38);
    pub const ELOOP: Errno = Errno(40);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ENOTSOCK: Errno = Errno(88);
    pub const EDESTADDRREQ: Errno = Errno(89);
    pub const EMSGSIZE: Errno = Errno(90);
    pub const ENOPROTOOPT: Errno = Errno(92);
    pub const EPROTONOSUPPORT: Errno = Errno(93);
    pub const ESOCKTNOSUPPORT: Errno = Errno(94);
    pub const EOPNOTSUPP: Errno = Errno(95);
    pub const EAFNOSUPPORT: Errno = Errno(97);
    pub const EADDRINUSE: Errno = Errno(98);
    pub const EADDRNOTAVAIL: Errno = Errno(99);
    pub const ENETDOWN: Errno = Errno(100);
    pub const ENETUNREACH: Errno = Errno(101);
    pub const ENOBUFS: Errno = Errno(105);
    pub const ENOTCONN: Errno = Errno(107);

    fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            1 => "operation not permitted",
            2 => "no such file or directory",
            3 => "no such process",
            5 => "I/O error",
            7 => "argument list too long",
            9 => "bad file descriptor",
            10 => "no child processes",
            11 => "try again",
            12 => "out of memory",
            14 => "bad address",
            16 => "device or resource busy",
            17 => "file exists",
            18 => "cross-device link",
            19 => "no such device (drive not mounted)",
            20 => "not a directory",
            21 => "is a directory",
            22 => "invalid argument",
            24 => "too many open files",
            25 => "not a terminal",
            27 => "file too large",
            28 => "no space left on device",
            29 => "illegal seek",
            30 => "read-only file system",
            32 => "broken pipe",
            34 => "result too large",
            36 => "file name too long (8.3 names only)",
            38 => "function not implemented",
            39 => "directory not empty",
            40 => "too many levels of symbolic links",
            88 => "not a socket",
            89 => "destination address required",
            90 => "message too long",
            92 => "protocol not available",
            93 => "protocol not supported",
            94 => "socket type not supported",
            95 => "operation not supported",
            97 => "address family not supported",
            98 => "address already in use",
            99 => "cannot assign requested address",
            100 => "network is down",
            101 => "network is unreachable",
            105 => "no buffer space available",
            107 => "not connected",
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
