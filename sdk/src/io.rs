//! Standard output/error and the `print!` family.

use core::fmt;

use crate::errno::{Result, check};
use crate::sys::{self, nr};

pub const STDIN: usize = 0;
pub const STDOUT: usize = 1;
pub const STDERR: usize = 2;

/// `write(fd, buf)`: returns how many bytes were written.
pub fn write(fd: usize, buf: &[u8]) -> Result<usize> {
    check(unsafe { sys::syscall3(nr::WRITE, fd, buf.as_ptr() as usize, buf.len()) })
}

/// Write all of `buf`, retrying on short writes.
pub fn write_all(fd: usize, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        let n = write(fd, buf)?;
        if n == 0 {
            return Err(crate::errno::Errno::EIO);
        }
        buf = &buf[n..];
    }
    Ok(())
}

/// Write straight to the kernel log (basicos-specific; shows up as
/// `[user] ...` at INFO level, alongside the kernel's own messages).
pub fn klog(msg: &str) -> Result<()> {
    check(unsafe { sys::syscall2(nr::KLOG, msg.as_ptr() as usize, msg.len()) }).map(|_| ())
}

/// Chunked writer behind `print!`: formats into a stack buffer and
/// flushes it as one `write` syscall, so a whole (short) line is a
/// single write. The kernel's `write` isn't atomic across tasks — a
/// fragment-at-a-time `println!` would interleave mid-line with every
/// other process's output. Longer lines flush in 256-byte chunks rather
/// than truncating.
struct Buffered {
    fd: usize,
    buf: [u8; 256],
    len: usize,
}

impl Buffered {
    fn flush(&mut self) {
        if self.len > 0 {
            let _ = write_all(self.fd, &self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl fmt::Write for Buffered {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut b = s.as_bytes();
        while !b.is_empty() {
            if self.len == self.buf.len() {
                self.flush();
            }
            let n = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
            self.len += n;
            b = &b[n..];
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(fd: usize, args: fmt::Arguments) {
    let mut w = Buffered {
        fd,
        buf: [0; 256],
        len: 0,
    };
    let _ = fmt::write(&mut w, args);
    w.flush();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDOUT, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDOUT, format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDERR, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($arg:tt)*) => { $crate::io::_print($crate::io::STDERR, format_args!("{}\n", format_args!($($arg)*))) };
}

/// An owned file descriptor, closed on drop. What `pipe`, `dup` and the
/// sockets hand back.
pub struct Fd {
    fd: usize,
}

impl Fd {
    /// Wrap a raw descriptor this process owns.
    pub fn from_raw(fd: usize) -> Fd {
        Fd { fd }
    }

    pub fn raw(&self) -> usize {
        self.fd
    }

    /// Give up ownership without closing.
    pub fn into_raw(self) -> usize {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        check(unsafe { sys::syscall3(nr::READ, self.fd, buf.as_mut_ptr() as usize, buf.len()) })
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        write(self.fd, buf)
    }

    pub fn write_all(&self, buf: &[u8]) -> Result<()> {
        write_all(self.fd, buf)
    }

    /// A second descriptor for the same open file/pipe/socket (shares
    /// its offset and flags).
    pub fn dup(&self) -> Result<Fd> {
        check(unsafe { sys::syscall1(nr::DUP, self.fd) }).map(Fd::from_raw)
    }

    /// Make descriptor number `target` refer to the same thing as this
    /// one (closing whatever `target` was) — how a shell wires up
    /// redirections before `exec`.
    pub fn dup_onto(&self, target: usize) -> Result<()> {
        check(unsafe { sys::syscall2(nr::DUP2, self.fd, target) }).map(|_| ())
    }

    /// Switch `O_NONBLOCK` on or off.
    pub fn set_nonblocking(&self, on: bool) -> Result<()> {
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        let flags = check(unsafe { sys::syscall3(nr::FCNTL, self.fd, F_GETFL, 0) })?;
        let new = if on { flags | crate::fs::O_NONBLOCK } else { flags & !crate::fs::O_NONBLOCK };
        check(unsafe { sys::syscall3(nr::FCNTL, self.fd, F_SETFL, new) }).map(|_| ())
    }

    /// Whether this is a terminal (the console).
    pub fn is_terminal(&self) -> bool {
        const TCGETS: usize = 0x5401;
        let mut termios = [0u8; 36];
        check(unsafe { sys::syscall3(nr::IOCTL, self.fd, TCGETS, termios.as_mut_ptr() as usize) }).is_ok()
    }

    /// Close now, reporting any error.
    pub fn close(self) -> Result<()> {
        let fd = self.into_raw();
        check(unsafe { sys::syscall1(nr::CLOSE, fd) }).map(|_| ())
    }
}

impl fmt::Debug for Fd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fd({})", self.fd)
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { sys::syscall1(nr::CLOSE, self.fd) };
    }
}

/// A unidirectional byte channel: `(read_end, write_end)`. Reading
/// returns 0 (EOF) once every write end — including copies inherited
/// across `fork` — is closed.
pub fn pipe() -> Result<(Fd, Fd)> {
    let mut fds = [0i32; 2];
    check(unsafe { sys::syscall1(nr::PIPE, fds.as_mut_ptr() as usize) })?;
    Ok((Fd::from_raw(fds[0] as usize), Fd::from_raw(fds[1] as usize)))
}

/// One entry for [`poll`].
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLHUP: i16 = 0x10;
pub const POLLNVAL: i16 = 0x20;

/// Wait until one of `fds` is ready. `timeout_ms < 0` waits forever.
/// Returns how many have non-zero `revents`.
pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> Result<usize> {
    check(unsafe {
        sys::syscall3(nr::POLL, fds.as_mut_ptr() as usize, fds.len(), timeout_ms as u32 as usize)
    })
}

/// Longest line [`read_line`] can receive in one call. Must match the
/// kernel's own limit (`syscall::console::MAX_LINE`) — its console
/// hands out a completed line whole or not at all, never torn across
/// calls to fit a short buffer (see that module's doc comment), so a
/// smaller buffer here would just never see a line that hit the limit.
pub const MAX_LINE: usize = 256;

/// Read one line of console input (cooked mode: echoed, edited with
/// backspace) into `line`, including the trailing newline. Returns the
/// bytes read; 0 means end of input (Ctrl-D).
///
/// One `read` syscall, not a byte at a time: the kernel's console
/// delivers a completed line to exactly one reader, whole, in a single
/// call — this is the only console on the machine, and other tasks
/// (the kernel's own resident shell, another process also reading fd 0)
/// may be reading it too. A byte-at-a-time loop would reopen the chance
/// for a completed line to be torn between two readers on a preemption
/// between calls, one of them left blocked forever short its own `\n`.
pub fn read_line(line: &mut alloc::string::String) -> Result<usize> {
    let mut buf = [0u8; MAX_LINE];
    let n = check(unsafe { sys::syscall3(nr::READ, STDIN, buf.as_mut_ptr() as usize, buf.len()) })?;
    line.push_str(&alloc::string::String::from_utf8_lossy(&buf[..n]));
    Ok(n)
}

/// Clear the console.
pub fn clear_screen() {
    unsafe { crate::sys::syscall0(crate::sys::nr::CLEAR) };
}
