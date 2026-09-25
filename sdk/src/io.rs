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
