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
    pub const LSEEK: usize = 8;
    pub const MMAP: usize = 9;
    pub const MUNMAP: usize = 11;
    pub const BRK: usize = 12;
    pub const YIELD: usize = 24;
    pub const GETPID: usize = 39;
    pub const FORK: usize = 57;
    pub const EXECVE: usize = 59;
    pub const EXIT: usize = 60;
    pub const WAIT4: usize = 61;
    pub const GETDENTS64: usize = 217;
    /// basicos-specific: write straight to the kernel log.
    pub const KLOG: usize = 0x400;
    /// basicos-specific: recursively list a mounted filesystem to the kernel log.
    pub const TREE: usize = 0x401;
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
