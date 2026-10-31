//! Raw syscall primitives for the basicos syscall ABI — the Rust
//! counterpart of `userland/syscalls.h` (numbers and semantics live in
//! `kernel/src/syscall/mod.rs`). Prefer the typed wrappers in [`crate::fs`],
//! [`crate::process`], [`crate::mem`] and [`crate::io`]; this layer is
//! public for anything they don't cover.
//!
//! ABI: RAX = syscall number, RDI/RSI/RDX/R10/R8/R9 = args 1-6, RAX =
//! return value (negative = -errno). `syscall` itself clobbers RCX/R11.
//!
//! **Every argument register is declared `inout(..) => _`, not a plain
//! `in(..)`.** This kernel's `syscall_return_to_user` only restores
//! RBX/RBP/R12-R15/RCX/R11/RSP — RDI/RSI/RDX/R8/R9/R10 come back as
//! whatever `dispatch()` left in them, unlike real Linux. A plain `in`
//! tells LLVM the register still holds its input afterward, which is
//! silently wrong here. (The C flavor of this bug — GCC deleting an
//! argument load outright — is written up in `userland/syscalls.h`.)

use core::arch::asm;

pub mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const OPEN: usize = 2;
    pub const CLOSE: usize = 3;
    pub const STAT: usize = 4;
    pub const FSTAT: usize = 5;
    pub const POLL: usize = 7;
    pub const LSEEK: usize = 8;
    pub const MMAP: usize = 9;
    pub const MUNMAP: usize = 11;
    pub const BRK: usize = 12;
    pub const IOCTL: usize = 16;
    pub const PREAD64: usize = 17;
    pub const PWRITE64: usize = 18;
    pub const READV: usize = 19;
    pub const WRITEV: usize = 20;
    pub const ACCESS: usize = 21;
    pub const PIPE: usize = 22;
    pub const YIELD: usize = 24;
    pub const DUP: usize = 32;
    pub const DUP2: usize = 33;
    pub const NANOSLEEP: usize = 35;
    pub const GETPID: usize = 39;
    pub const SOCKET: usize = 41;
    pub const CONNECT: usize = 42;
    pub const SENDTO: usize = 44;
    pub const RECVFROM: usize = 45;
    pub const SHUTDOWN: usize = 48;
    pub const BIND: usize = 49;
    pub const GETSOCKNAME: usize = 51;
    pub const GETPEERNAME: usize = 52;
    pub const SETSOCKOPT: usize = 54;
    pub const GETSOCKOPT: usize = 55;
    pub const FORK: usize = 57;
    pub const EXECVE: usize = 59;
    pub const EXIT: usize = 60;
    pub const WAIT4: usize = 61;
    pub const KILL: usize = 62;
    pub const UNAME: usize = 63;
    pub const FCNTL: usize = 72;
    pub const FSYNC: usize = 74;
    pub const TRUNCATE: usize = 76;
    pub const FTRUNCATE: usize = 77;
    pub const GETCWD: usize = 79;
    pub const CHDIR: usize = 80;
    pub const RENAME: usize = 82;
    pub const MKDIR: usize = 83;
    pub const RMDIR: usize = 84;
    pub const LSTAT: usize = 6;
    pub const LINK: usize = 86;
    pub const UNLINK: usize = 87;
    pub const SYMLINK: usize = 88;
    pub const READLINK: usize = 89;
    pub const CHMOD: usize = 90;
    pub const FCHMOD: usize = 91;
    pub const STATFS: usize = 137;
    pub const SYNC: usize = 162;
    pub const UMASK: usize = 95;
    pub const GETTIMEOFDAY: usize = 96;
    pub const SYSINFO: usize = 99;
    pub const GETUID: usize = 102;
    pub const GETPPID: usize = 110;
    pub const TIME: usize = 201;
    pub const GETDENTS64: usize = 217;
    pub const CLOCK_GETTIME: usize = 228;
    pub const CLOCK_NANOSLEEP: usize = 230;
    pub const EXIT_GROUP: usize = 231;
    pub const OPENAT: usize = 257;
    pub const DUP3: usize = 292;
    pub const PIPE2: usize = 293;
    pub const GETRANDOM: usize = 318;
    /// basicos-specific: write straight to the kernel log.
    pub const KLOG: usize = 0x400;
    /// basicos-specific: recursively list a mounted filesystem to the kernel log.
    pub const TREE: usize = 0x401;
    /// basicos-specific: clear the console.
    pub const CLEAR: usize = 0x403;
}

#[inline(always)]
pub unsafe fn syscall0(n: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall1(n: usize, a1: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _, inout("rsi") a2 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _, inout("rsi") a2 => _, inout("rdx") a3 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _, inout("rsi") a2 => _, inout("rdx") a3 => _,
            inout("r10") a4 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _, inout("rsi") a2 => _, inout("rdx") a3 => _,
            inout("r10") a4 => _, inout("r8") a5 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}

#[inline(always)]
pub unsafe fn syscall6(
    n: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let ret: isize;
    unsafe {
        asm!("syscall",
            inlateout("rax") n as isize => ret,
            inout("rdi") a1 => _, inout("rsi") a2 => _, inout("rdx") a3 => _,
            inout("r10") a4 => _, inout("r8") a5 => _, inout("r9") a6 => _,
            lateout("rcx") _, lateout("r11") _,
            options(nostack));
    }
    ret
}
