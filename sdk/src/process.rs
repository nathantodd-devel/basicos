//! Processes: exit, fork, exec, wait.

use alloc::vec::Vec;

use crate::errno::{Errno, Result, check};
use crate::sys::{self, nr};

pub type Pid = usize;

pub fn exit(status: i32) -> ! {
    unsafe { sys::syscall1(nr::EXIT, status as usize) };
    // The kernel never returns from exit; this just satisfies `!`.
    loop {
        core::hint::spin_loop();
    }
}

pub fn getpid() -> Pid {
    unsafe { sys::syscall0(nr::GETPID) as Pid }
}

/// Give up the rest of this timeslice.
pub fn yield_now() {
    unsafe { sys::syscall0(nr::YIELD) };
}

/// Which side of a [`fork`] you're on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fork {
    /// In the original process; carries the child's pid.
    Parent(Pid),
    /// In the new child.
    Child,
}

/// Duplicate this process. The child gets a full copy of the address
/// space (not copy-on-write) and resumes right here, with `Fork::Child`.
pub fn fork() -> Result<Fork> {
    let pid = check(unsafe { sys::syscall0(nr::FORK) })?;
    Ok(if pid == 0 { Fork::Child } else { Fork::Parent(pid) })
}

/// Wait for a child to exit (`Some(pid)`: that one; `None`: any) and reap
/// it. Returns its pid and exit status; `Err(ECHILD)` if there is no
/// matching child at all.
pub fn wait(pid: Option<Pid>) -> Result<(Pid, i32)> {
    let mut status = 0i32;
    let target = pid.unwrap_or(usize::MAX); // -1 == any child
    let r = check(unsafe {
        sys::syscall3(nr::WAIT4, target, &mut status as *mut i32 as usize, 0)
    })?;
    Ok((r, status))
}

/// Replace this process with `path`, passing `args` as its `argv`
/// (conventionally `args[0]` is the program's own name). Only returns on
/// failure — on success this program is gone.
pub fn exec(path: &str, args: &[&str]) -> Errno {
    // NUL-terminated copies of everything, plus a NULL-terminated
    // pointer array, all kept alive across the syscall.
    let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(args.len() + 1);
    let mut push = |s: &str| {
        let mut v = Vec::with_capacity(s.len() + 1);
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        bufs.push(v);
    };
    push(path);
    for a in args {
        push(a);
    }
    let mut argv: Vec<usize> = bufs[1..].iter().map(|b| b.as_ptr() as usize).collect();
    argv.push(0);

    let r = unsafe {
        sys::syscall3(nr::EXECVE, bufs[0].as_ptr() as usize, argv.as_ptr() as usize, 0)
    };
    Errno(if r < 0 { -r as i32 } else { 0 })
}

/// The parent's pid (0 if it has none, e.g. `init`'s launcher).
pub fn getppid() -> Pid {
    unsafe { sys::syscall0(nr::GETPPID) as Pid }
}

pub const SIGHUP: usize = 1;
pub const SIGINT: usize = 2;
pub const SIGKILL: usize = 9;
pub const SIGTERM: usize = 15;

/// Send a signal. There are no handlers: fatal signals terminate the
/// target (its exit status becomes `128 + sig`), ignorable ones
/// (`SIGCHLD`, `SIGCONT`, ...) are dropped, and signal 0 just checks
/// that the process exists.
pub fn kill(pid: Pid, sig: usize) -> Result<()> {
    check(unsafe { sys::syscall2(nr::KILL, pid, sig) }).map(|_| ())
}

/// `uname`: system name and version strings.
#[derive(Debug, Clone)]
pub struct Uname {
    pub sysname: alloc::string::String,
    pub release: alloc::string::String,
    pub machine: alloc::string::String,
}

pub fn uname() -> Result<Uname> {
    let mut b = [0u8; 65 * 6];
    check(unsafe { sys::syscall1(nr::UNAME, b.as_mut_ptr() as usize) })?;
    let field = |i: usize| {
        let f = &b[i * 65..(i + 1) * 65];
        let end = f.iter().position(|&c| c == 0).unwrap_or(f.len());
        alloc::string::String::from_utf8_lossy(&f[..end]).into_owned()
    };
    Ok(Uname { sysname: field(0), release: field(2), machine: field(4) })
}

/// Fill `buf` with pseudo-random bytes (xorshift seeded from the TSC —
/// fine for jitter and test data, **not** for cryptography).
pub fn getrandom(buf: &mut [u8]) -> Result<()> {
    check(unsafe { sys::syscall3(nr::GETRANDOM, buf.as_mut_ptr() as usize, buf.len(), 0) }).map(|_| ())
}
