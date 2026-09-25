//! Memory syscalls: `brk` (the process heap) and anonymous `mmap`.
//!
//! Each process has a fixed-size window for each (see
//! `kernel/src/mem/paging.rs`'s process zone: a 5 MiB heap and a 6 MiB
//! mmap arena). The arena is bump-allocated — `munmap` frees the
//! physical pages but the address range isn't reused — so prefer
//! `Vec`/`Box` (which go through [`crate::heap`]'s allocator, and reuse
//! freed memory) over calling `mmap` directly for anything short-lived.

use crate::errno::{Errno, Result, check};
use crate::sys::{self, nr};

/// `brk(addr)`: set the program break to `addr` and return the resulting
/// break. `addr == 0` just queries. Never fails outright — a grow the
/// kernel can't satisfy leaves the break where it was and returns that,
/// so callers compare the result to what they asked for.
pub fn brk(addr: usize) -> usize {
    unsafe { sys::syscall1(nr::BRK, addr) as usize }
}

const PROT_WRITE: usize = 0x2;
const PROT_READ: usize = 0x1;
const MAP_ANONYMOUS: usize = 0x20;

/// Map `len` bytes of fresh, zeroed anonymous memory (rounded up to
/// whole pages by the kernel). Anonymous only — the kernel has no page
/// cache to back a file mapping with.
pub fn mmap(len: usize, writable: bool) -> Result<*mut u8> {
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    let prot = if writable { PROT_READ | PROT_WRITE } else { PROT_READ };
    let r = unsafe {
        // fd must be -1 (all ones) for MAP_ANONYMOUS; addr hint and
        // offset are ignored by the kernel.
        sys::syscall6(nr::MMAP, 0, len, prot, MAP_ANONYMOUS, usize::MAX, 0)
    };
    check(r).map(|a| a as *mut u8)
}

/// Unmap pages previously returned by [`mmap`].
///
/// # Safety
/// `ptr..ptr+len` must not be used again, and must have come from `mmap`.
pub unsafe fn munmap(ptr: *mut u8, len: usize) -> Result<()> {
    check(unsafe { sys::syscall2(nr::MUNMAP, ptr as usize, len) }).map(|_| ())
}
