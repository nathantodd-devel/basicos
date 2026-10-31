//! Syscall dispatch and handlers.
//!
//! This module defines syscall numbers and implements the handlers.
//! The dispatch table matches Linux syscall numbers where practical.
//!
//! # Syscall ABI
//!
//! - RAX: syscall number
//! - RDI, RSI, RDX, R10, R8, R9: arguments 1-6
//! - RAX: return value (negative values indicate errors)

use crate::{pr_debug, pr_info};
#[cfg(feature = "storage")]
use crate::fs;
#[cfg(feature = "storage")]
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

// `pub(crate)`, not private: `shell` also reads stdin through this one
// line discipline (see its own `read_line`) — otherwise its direct
// driver reads and this module's independent byte-queue draining would
// each silently steal bytes meant for the other.
pub(crate) mod console;
pub mod fd;
#[cfg(feature = "storage")]
mod file;
mod io;
mod misc;
#[cfg(feature = "storage")]
mod procfs;
#[cfg(feature = "net")]
mod sock;
mod uaccess;

// ============================================================================
// Syscall Numbers (Linux x86_64 numbering)
// ============================================================================

pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    pub const YIELD: u64 = 24; // sched_yield
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const SENDTO: u64 = 44;
    pub const RECVFROM: u64 = 45;
    pub const SHUTDOWN: u64 = 48;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const GETPEERNAME: u64 = 52;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    pub const FORK: u64 = 57;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const TRUNCATE: u64 = 76;
    pub const FTRUNCATE: u64 = 77;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const LINK: u64 = 86;
    pub const UNLINK: u64 = 87;
    pub const SYMLINK: u64 = 88;
    pub const READLINK: u64 = 89;
    pub const CHMOD: u64 = 90;
    pub const FCHMOD: u64 = 91;
    pub const UMASK: u64 = 95;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const SYSINFO: u64 = 99;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const GETPPID: u64 = 110;
    pub const STATFS: u64 = 137;
    pub const FSTATFS: u64 = 138;
    pub const SYNC: u64 = 162;
    pub const TIME: u64 = 201;
    pub const GETDENTS64: u64 = 217;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const GETRANDOM: u64 = 318;

    // basicos-specific syscalls (start at 0x400 to avoid Linux conflicts)
    pub const KLOG: u64 = 0x400; // Write to kernel log
    pub const TREE: u64 = 0x401; // Recursively list a mounted filesystem to the kernel log
    pub const KPANIC: u64 = 0x402;
    pub const CLEAR: u64 = 0x403; // Clear the console
}

/// `mmap()` flags (arg4) — same bit values as Linux x86_64.
#[cfg(feature = "elf-loader")]
pub mod mmap_flags {
    pub const PROT_WRITE: u64 = 0x2;
    pub const MAP_ANONYMOUS: u64 = 0x20;
}

/// `TREE` syscall targets (arg1).
pub mod tree_target {
    pub const DISK: u64 = 0;
    pub const INITRD: u64 = 1;
    pub const RAMDISK: u64 = 2;
}

/// `lseek()` whence values (arg3).
pub mod whence {
    pub const SEEK_SET: u64 = 0;
    pub const SEEK_CUR: u64 = 1;
    pub const SEEK_END: u64 = 2;
}

/// `open()` flags (arg2) — same bit values as Linux x86_64, so real C
/// programs' `<fcntl.h>` constants just work.
#[allow(dead_code)]
pub mod open_flags {
    pub const O_RDONLY: u64 = 0o0;
    pub const O_WRONLY: u64 = 0o1;
    pub const O_RDWR: u64 = 0o2;
    pub const O_ACCMODE: u64 = 0o3;
    pub const O_CREAT: u64 = 0o100;
    pub const O_EXCL: u64 = 0o200;
    pub const O_TRUNC: u64 = 0o1000;
    pub const O_APPEND: u64 = 0o2000;
    pub const O_NONBLOCK: u64 = 0o4000;
    pub const O_DIRECTORY: u64 = 0o200000;
    pub const O_CLOEXEC: u64 = 0o2000000;
}

// ============================================================================
// Error Codes (negated in return value; Linux numbering)
// ============================================================================

#[allow(dead_code)]
pub mod errno {
    pub const EPERM: i64 = -1; // Operation not permitted
    pub const ENOENT: i64 = -2; // No such file or directory
    pub const ESRCH: i64 = -3; // No such process
    pub const EIO: i64 = -5; // I/O error
    pub const EBADF: i64 = -9; // Bad file descriptor
    pub const ECHILD: i64 = -10; // No child processes
    pub const EAGAIN: i64 = -11; // Try again (also: would block)
    pub const ENOMEM: i64 = -12; // Out of memory
    pub const EFAULT: i64 = -14; // Bad address
    pub const EBUSY: i64 = -16; // Device or resource busy
    pub const EEXIST: i64 = -17; // File exists
    pub const EXDEV: i64 = -18; // Cross-device link
    pub const ENODEV: i64 = -19; // No such device (filesystem not mounted)
    pub const ENOTDIR: i64 = -20; // Not a directory
    pub const EISDIR: i64 = -21; // Is a directory
    pub const EINVAL: i64 = -22; // Invalid argument
    pub const EMFILE: i64 = -24; // Too many open files
    pub const ENOTTY: i64 = -25; // Not a typewriter
    pub const ENOSPC: i64 = -28; // No space left on device
    pub const ESPIPE: i64 = -29; // Illegal seek
    pub const EROFS: i64 = -30; // Read-only file system
    pub const EPIPE: i64 = -32; // Broken pipe
    pub const ERANGE: i64 = -34; // Result too large
    pub const ENAMETOOLONG: i64 = -36; // File name too long
    pub const ENOSYS: i64 = -38; // Function not implemented
    pub const ENOTEMPTY: i64 = -39; // Directory not empty
    pub const ENOTSOCK: i64 = -88; // Socket operation on non-socket
    pub const EDESTADDRREQ: i64 = -89; // Destination address required
    pub const EMSGSIZE: i64 = -90; // Message too long
    pub const ENOPROTOOPT: i64 = -92; // Protocol not available
    pub const EPROTONOSUPPORT: i64 = -93; // Protocol not supported
    pub const ESOCKTNOSUPPORT: i64 = -94; // Socket type not supported
    pub const EOPNOTSUPP: i64 = -95; // Operation not supported
    pub const EAFNOSUPPORT: i64 = -97; // Address family not supported
    pub const EADDRINUSE: i64 = -98; // Address already in use
    pub const EADDRNOTAVAIL: i64 = -99; // Cannot assign requested address
    pub const ENETUNREACH: i64 = -101; // Network is unreachable
    pub const ENOBUFS: i64 = -105; // No buffer space available
    pub const ENOTCONN: i64 = -107; // Transport endpoint is not connected
    pub const E2BIG: i64 = -7; // Argument list too long
    pub const EFBIG: i64 = -27; // File too large
    pub const ELOOP: i64 = -40; // Too many symbolic links
}

/// Milliseconds since boot, at PIT-tick resolution.
fn now_ms() -> u64 {
    use crate::arch::interrupts::{TIMER_HZ, TIMER_TICKS};
    TIMER_TICKS.load(core::sync::atomic::Ordering::Relaxed) * 1000 / TIMER_HZ as u64
}

/// One step of a blocking wait: hand the CPU to someone else if there is
/// a scheduler, else just spin.
fn relax() {
    #[cfg(feature = "scheduler")]
    crate::sched::yield_now();
    #[cfg(not(feature = "scheduler"))]
    core::hint::spin_loop();
}

// ============================================================================
// Syscall Dispatch
// ============================================================================

/// Main syscall dispatcher. Called from assembly with arguments already set up.
///
/// # Arguments
/// - `number`: syscall number (from RAX)
/// - `arg1`-`arg6`: syscall arguments
///
/// # Returns
/// Syscall result (negative = error)
#[no_mangle]
pub extern "C" fn dispatch(
    number: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    #[cfg_attr(not(feature = "elf-loader"), allow(unused_variables))] arg6: u64,
) -> i64 {
    // Not every arm uses every argument in every feature set.
    let _ = (arg4, arg5);
    match number {
        // --- file descriptors: work on any fd -----------------------------
        nr::READ => io::sys_read(arg1, arg2, arg3),
        nr::WRITE => io::sys_write(arg1, arg2, arg3),
        nr::CLOSE => io::sys_close(arg1),
        nr::FSTAT => io::sys_fstat(arg1, arg2),
        nr::READV => io::sys_readv(arg1, arg2, arg3),
        nr::WRITEV => io::sys_writev(arg1, arg2, arg3),
        nr::IOCTL => io::sys_ioctl(arg1, arg2, arg3),
        nr::PIPE => io::sys_pipe(arg1),
        nr::PIPE2 => io::sys_pipe2(arg1, arg2),
        nr::DUP => io::sys_dup(arg1),
        nr::DUP2 => io::sys_dup2(arg1, arg2),
        nr::DUP3 => io::sys_dup3(arg1, arg2, arg3),
        nr::FCNTL => io::sys_fcntl(arg1, arg2, arg3),
        nr::UMASK => io::sys_umask(arg1),
        nr::POLL => misc::sys_poll(arg1, arg2, arg3),

        // --- files and directories ----------------------------------------
        #[cfg(feature = "storage")]
        nr::OPEN => file::sys_open(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::OPENAT => file::sys_openat(arg1, arg2, arg3, arg4),
        #[cfg(feature = "storage")]
        nr::STAT => file::sys_stat(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::LSTAT => file::sys_lstat(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::LINK => file::sys_link(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::SYMLINK => file::sys_symlink(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::READLINK => file::sys_readlink(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::CHMOD => file::sys_chmod(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::FCHMOD => file::sys_fchmod(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::STATFS => file::sys_statfs(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::FSTATFS => file::sys_fstatfs(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::SYNC => file::sys_sync(),
        #[cfg(feature = "storage")]
        nr::NEWFSTATAT => file::sys_newfstatat(arg1, arg2, arg3, arg4),
        #[cfg(feature = "storage")]
        nr::LSEEK => file::sys_lseek(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::PREAD64 => file::sys_pread64(arg1, arg2, arg3, arg4),
        #[cfg(feature = "storage")]
        nr::PWRITE64 => file::sys_pwrite64(arg1, arg2, arg3, arg4),
        #[cfg(feature = "storage")]
        nr::ACCESS => file::sys_access(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::FSYNC => file::sys_fsync(arg1),
        #[cfg(feature = "storage")]
        nr::TRUNCATE => file::sys_truncate(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::FTRUNCATE => file::sys_ftruncate(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::GETCWD => file::sys_getcwd(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::CHDIR => file::sys_chdir(arg1),
        #[cfg(feature = "storage")]
        nr::RENAME => file::sys_rename(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::MKDIR => file::sys_mkdir(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::MKDIRAT => file::sys_mkdirat(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::RMDIR => file::sys_rmdir(arg1),
        #[cfg(feature = "storage")]
        nr::UNLINK => file::sys_unlink(arg1),
        #[cfg(feature = "storage")]
        nr::UNLINKAT => file::sys_unlinkat(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::GETDENTS64 => file::sys_getdents64(arg1, arg2, arg3),

        // --- sockets (UDP) ---------------------------------------------------
        #[cfg(feature = "net")]
        nr::SOCKET => sock::sys_socket(arg1, arg2, arg3),
        #[cfg(feature = "net")]
        nr::BIND => sock::sys_bind(arg1, arg2, arg3),
        #[cfg(feature = "net")]
        nr::CONNECT => sock::sys_connect(arg1, arg2, arg3),
        #[cfg(feature = "net")]
        nr::SENDTO => sock::sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        #[cfg(feature = "net")]
        nr::RECVFROM => sock::sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        #[cfg(feature = "net")]
        nr::GETSOCKNAME => sock::sys_getsockname(arg1, arg2, arg3),
        #[cfg(feature = "net")]
        nr::GETPEERNAME => sock::sys_getpeername(arg1, arg2, arg3),
        #[cfg(feature = "net")]
        nr::SETSOCKOPT => sock::sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        #[cfg(feature = "net")]
        nr::GETSOCKOPT => sock::sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        #[cfg(feature = "net")]
        nr::SHUTDOWN => sock::sys_shutdown(arg1, arg2),
        #[cfg(feature = "net")]
        nr::LISTEN | nr::ACCEPT => sock::sys_listen_accept(arg1),

        // --- processes ---------------------------------------------------------
        nr::EXIT | nr::EXIT_GROUP => sys_exit(arg1 as i32),
        nr::GETPID => sys_getpid(),
        nr::GETPPID => misc::sys_getppid(),
        nr::YIELD => sys_yield(),
        #[cfg(feature = "elf-loader")]
        nr::MMAP => sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        #[cfg(feature = "elf-loader")]
        nr::MUNMAP => sys_munmap(arg1, arg2),
        #[cfg(feature = "elf-loader")]
        nr::BRK => sys_brk(arg1),
        #[cfg(feature = "elf-loader")]
        nr::FORK => sys_fork(),
        #[cfg(feature = "elf-loader")]
        nr::EXECVE => sys_execve(arg1, arg2, arg3),
        #[cfg(feature = "elf-loader")]
        nr::WAIT4 => sys_wait4(arg1, arg2, arg3),
        #[cfg(feature = "elf-loader")]
        nr::KILL => misc::sys_kill(arg1, arg2),
        nr::GETUID | nr::GETGID | nr::GETEUID | nr::GETEGID => misc::sys_root_id(),

        // --- time and system info ---------------------------------------------
        nr::NANOSLEEP => misc::sys_nanosleep(arg1, arg2),
        nr::CLOCK_NANOSLEEP => misc::sys_clock_nanosleep(arg1, arg2, arg3, arg4),
        nr::CLOCK_GETTIME => misc::sys_clock_gettime(arg1, arg2),
        nr::CLOCK_GETRES => misc::sys_clock_getres(arg1, arg2),
        nr::GETTIMEOFDAY => misc::sys_gettimeofday(arg1, arg2),
        nr::TIME => misc::sys_time(arg1),
        nr::UNAME => misc::sys_uname(arg1),
        nr::SYSINFO => misc::sys_sysinfo(arg1),
        nr::GETRANDOM => misc::sys_getrandom(arg1, arg2, arg3),

        // --- basicos-specific --------------------------------------------------
        nr::KLOG => sys_klog(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::TREE => sys_tree(arg1),
        // The no-scheduler demo's panic test. Never reachable from a real
        // process: a program must not be able to take the kernel down.
        #[cfg(not(feature = "scheduler"))]
        nr::KPANIC => sys_kpanic(),
        nr::CLEAR => sys_clear(),
        _ => {
            pr_debug!("Unknown syscall: {}", number);
            errno::ENOSYS
        }
    }
}

// ============================================================================
// Syscall Implementations
// ============================================================================

/// exit(status) - Terminate the current process. Frees its address space
/// immediately and turns it into a zombie for a parent's `wait`/`waitpid`
/// to reap (see `sched::exit_process`) — or, without `elf-loader` (so no
/// real process table exists at all), just halts as before.
fn sys_exit(status: i32) -> i64 {
    pr_debug!("Process exited with status: {}", status);
    #[cfg(feature = "elf-loader")]
    crate::sched::exit_process(status);
    #[cfg(not(feature = "elf-loader"))]
    loop {
        unsafe {
            core::arch::asm!("cli", "hlt");
        }
    }
}

/// getpid() - Get the current process ID.
fn sys_getpid() -> i64 {
    #[cfg(feature = "elf-loader")]
    {
        crate::sched::current_pid() as i64
    }
    #[cfg(not(feature = "elf-loader"))]
    {
        1
    }
}

/// yield() - Yield the CPU to another process.
fn sys_yield() -> i64 {
    #[cfg(feature = "scheduler")]
    crate::sched::yield_now();
    0
}

/// fork() - Duplicate the calling process. Returns the child's pid in
/// the parent; the child never actually executes this function (it
/// resumes directly via `arch::x86_64::syscall::fork_child_entry` with
/// RAX already forced to 0 — see `sched::fork_current`'s doc comment).
#[cfg(feature = "elf-loader")]
fn sys_fork() -> i64 {
    match crate::sched::fork_current() {
        Some(pid) => pid as i64,
        None => errno::EAGAIN,
    }
}

/// Read a NULL-terminated array of user pointers-to-C-strings (an
/// `argv`/`envp`-shaped array) into owned `String`s, validating every
/// pointer. At most `MAX_ARGS` entries.
#[cfg(feature = "elf-loader")]
fn read_user_argv(ptr: u64) -> Result<Vec<String>, i64> {
    const MAX_ARGS: usize = 32;
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    for i in 0..MAX_ARGS as u64 {
        let p: u64 = uaccess::get(ptr + i * 8)?;
        if p == 0 {
            return Ok(out);
        }
        out.push(uaccess::cstring(p)?);
    }
    Err(errno::E2BIG)
}

/// execve(path, argv, envp) - Replace the calling process's own image
/// with a freshly loaded ELF binary (same pid, same parent — see
/// `sched::exec_current`). `envp` is read but ignored: there's no
/// environment-variable support here, every exec'd process just gets an
/// empty environment.
#[cfg(feature = "elf-loader")]
fn sys_execve(path_ptr: u64, argv_ptr: u64, _envp_ptr: u64) -> i64 {
    let path = match uaccess::cstring(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let argv = match read_user_argv(argv_ptr) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    match crate::sched::exec_current(&path, &argv_refs) {
        Ok(()) => 0, // never actually observed — the new program starts fresh
        Err(e) => {
            pr_debug!("execve: {:?}", e);
            errno::ENOENT
        }
    }
}

/// wait4(pid, status_ptr, options, rusage) - Block until a child exits
/// (`pid > 0`: that specific child; otherwise: any child), reap it, and
/// write its exit status to `status_ptr` (if non-null). `options` and
/// `rusage` (arg4, not passed here) aren't supported. Returns the reaped
/// child's pid, or `ECHILD` if the caller has no matching child at all.
#[cfg(feature = "elf-loader")]
fn sys_wait4(pid: u64, status_ptr: u64, _options: u64) -> i64 {
    let target = if (pid as i64) > 0 { Some(pid) } else { None };
    match crate::sched::wait_for_child(target) {
        Some((child_pid, status)) => {
            if status_ptr != 0 {
                if let Err(e) = uaccess::put(status_ptr, status) {
                    return e;
                }
            }
            child_pid as i64
        }
        None => errno::ECHILD,
    }
}

/// mmap(addr, len, prot, flags, fd, offset) - Map `len` bytes of fresh,
/// zeroed anonymous memory into the calling process's own mmap arena
/// (see `sched::proc_mmap`). `addr` (a placement hint) and `offset` are
/// ignored; only `MAP_ANONYMOUS` is supported — there's no page cache
/// here to back a file-backed mapping with, so anything else is EINVAL.
#[cfg(feature = "elf-loader")]
fn sys_mmap(_addr: u64, len: u64, prot: u64, flags: u64, fd: u64, _offset: u64) -> i64 {
    use mmap_flags::*;
    if flags & MAP_ANONYMOUS == 0 || fd as i64 != -1 {
        return errno::EINVAL;
    }
    if len == 0 {
        return errno::EINVAL;
    }
    match crate::sched::proc_mmap(len as usize, prot & PROT_WRITE != 0) {
        Some(addr) => addr as i64,
        None => errno::ENOMEM,
    }
}

/// munmap(addr, len) - Unmap and free pages the calling process mapped
/// via `mmap`.
#[cfg(feature = "elf-loader")]
fn sys_munmap(addr: u64, len: u64) -> i64 {
    if crate::sched::proc_munmap(addr, len as usize) {
        0
    } else {
        errno::EINVAL
    }
}

/// brk(addr) - Grow or shrink the calling process's heap; `addr == 0`
/// queries the current break. Always returns the resulting break (never
/// a negative errno — matches the real `brk()` ABI).
#[cfg(feature = "elf-loader")]
fn sys_brk(addr: u64) -> i64 {
    crate::sched::proc_brk(addr) as i64
}

/// klog(buf, len) - Write directly to kernel log (basicos-specific).
fn sys_klog(buf: u64, len: u64) -> i64 {
    let slice = match uaccess::slice(buf, len as usize) {
        Ok(slice) => slice,
        Err(e) => return e,
    };

    if let Ok(s) = core::str::from_utf8(slice) {
        pr_info!("[user] {}", s);
        len as i64
    } else {
        errno::EINVAL
    }
}

/// tree(target) - Recursively list a mounted FAT32 filesystem to the
/// kernel log (basicos-specific). `target` is one of `tree_target`'s
/// `DISK`/`INITRD`/`RAMDISK` constants.
///
/// Ring 3 can't walk the filesystem itself — no port I/O, no access to
/// the kernel heap or the `fs::Fat32Fs` handle — so this does the whole
/// walk in kernel mode and just reports success/failure back to userspace.
#[cfg(feature = "storage")]
fn sys_tree(target: u64) -> i64 {
    let (label, drive) = match target {
        tree_target::DISK => ("disk", fs::Drive::Disk),
        tree_target::INITRD => ("initrd", fs::Drive::Initrd),
        tree_target::RAMDISK => ("ramdisk", fs::Drive::Ramdisk),
        _ => return errno::EINVAL,
    };
    let result = fs::with_drive(drive, |f| print_tree(f, "/", 0));
    match result {
        Some(Ok(())) => 0,
        Some(Err(e)) => {
            pr_info!("[user] tree {}: error {:?}", label, e);
            errno::EIO
        }
        None => {
            pr_info!("[user] tree {}: not mounted", label);
            errno::ENODEV
        }
    }
}

/// Recursively print `path`'s contents (indented by `depth`) to the
/// kernel log, descending into subdirectories.
#[cfg(feature = "storage")]
fn print_tree(fs: &mut dyn fs::FileSystem, path: &str, depth: usize) -> Result<(), fs::FsError> {
    if depth == 0 {
        pr_info!("[user] /");
    }
    for entry in fs.read_dir_at(path)? {
        let indent = "  ".repeat(depth + 1);
        if entry.is_dir {
            pr_info!("[user] {}{}/", indent, entry.name);
            let child_path = if path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", path, entry.name)
            };
            print_tree(fs, &child_path, depth + 1)?;
        } else {
            pr_info!("[user] {}{} ({} bytes)", indent, entry.name, entry.size);
        }
    }
    Ok(())
}

#[cfg(not(feature = "scheduler"))]
fn sys_kpanic() -> i64 {
    panic!("Test panic invoked from usermode");
}

/// clear() - Clear the VGA screen, and the terminal on the serial port.
fn sys_clear() -> i64 {
    crate::drivers::vga::clear_screen();
    for &b in b"\x1b[2J\x1b[H" {
        crate::drivers::serial::write_byte(b);
    }
    0
}

// ============================================================================
// User-space Syscall Interface (for the test program)
// ============================================================================

/// Invoke a syscall from user space (for testing in user_mode_stub).
/// This is meant to be called from ring 3 code.
#[inline(always)]
pub fn syscall0(number: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

#[inline(always)]
pub fn syscall1(number: u64, arg1: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

#[inline(always)]
pub fn syscall2(number: u64, arg1: u64, arg2: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

#[inline(always)]
pub fn syscall3(number: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

// syscall4/5/6 round out the interface now that `syscall_entry` actually
// threads arg4-6 (R10/R8/R9) through to `dispatch` instead of zeroing
// them — see the fix in `arch::x86_64::syscall`. Nothing here needs more
// than 3 args yet, so these are unused for now.
#[inline(always)]
#[allow(dead_code)]
pub fn syscall4(number: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

#[inline(always)]
#[allow(dead_code)]
pub fn syscall5(number: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}

#[inline(always)]
#[allow(dead_code)]
pub fn syscall6(
    number: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            in("r9") arg6,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    ret
}
