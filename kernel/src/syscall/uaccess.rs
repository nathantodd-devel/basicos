//! Checked access to user memory.
//!
//! The older handlers dereference user pointers raw, so a bad pointer
//! from a process page-faults *the kernel*. Everything new goes through
//! here instead: with the `elf-loader` (real per-process address spaces)
//! a pointer is accepted only if the whole range lies inside the process
//! zone and every page of it is actually mapped in the calling
//! process's page tables; otherwise the syscall gets `EFAULT`. Without
//! it (the one-shot ring-3 demo runs on the kernel's own mapping) only
//! null is rejected.

use super::errno;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "elf-loader")]
fn range_mapped(ptr: u64, len: usize) -> bool {
    use crate::mem::paging::{PageMapper, VirtualAddress, PAGE_SIZE, PROC_ZONE_END, PROC_ZONE_START};
    let Some(end) = ptr.checked_add(len as u64) else {
        return false;
    };
    if ptr < PROC_ZONE_START as u64 || end > PROC_ZONE_END as u64 {
        return false;
    }
    let space = PageMapper::for_address_space(crate::mem::paging::read_cr3());
    let mut page = ptr & !(PAGE_SIZE as u64 - 1);
    while page < end {
        // Safety: read-only walk of this process's own page tables.
        if unsafe { space.translate(VirtualAddress::new(page)) }.is_none() {
            return false;
        }
        page += PAGE_SIZE as u64;
    }
    true
}

#[cfg(not(feature = "elf-loader"))]
fn range_mapped(ptr: u64, _len: usize) -> bool {
    ptr != 0
}

/// Whether `[ptr, ptr+len)` is a valid user range. An empty range is
/// always fine (`read(fd, NULL, 0)` is legal).
pub fn ok(ptr: u64, len: usize) -> bool {
    len == 0 || range_mapped(ptr, len)
}

pub fn slice<'a>(ptr: u64, len: usize) -> Result<&'a [u8], i64> {
    if len == 0 {
        return Ok(&[]);
    }
    if !ok(ptr, len) {
        return Err(errno::EFAULT);
    }
    // Safety: just checked the whole range is mapped in this process.
    Ok(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
}

pub fn slice_mut<'a>(ptr: u64, len: usize) -> Result<&'a mut [u8], i64> {
    if len == 0 {
        return Ok(&mut []);
    }
    if !ok(ptr, len) {
        return Err(errno::EFAULT);
    }
    // Safety: as in `slice`.
    Ok(unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) })
}

/// Copy a plain-old-data value out to user memory.
pub fn put<T: Copy>(ptr: u64, value: T) -> Result<(), i64> {
    let size = core::mem::size_of::<T>();
    if !ok(ptr, size) {
        return Err(errno::EFAULT);
    }
    // Safety: range checked; user pointers have no alignment guarantee.
    unsafe { core::ptr::write_unaligned(ptr as *mut T, value) };
    Ok(())
}

/// Copy a plain-old-data value in from user memory.
pub fn get<T: Copy>(ptr: u64) -> Result<T, i64> {
    let size = core::mem::size_of::<T>();
    if !ok(ptr, size) {
        return Err(errno::EFAULT);
    }
    // Safety: as in `put`.
    Ok(unsafe { core::ptr::read_unaligned(ptr as *const T) })
}

/// Longest path accepted, terminator included.
pub const PATH_MAX: usize = 1024;

/// Copy a NUL-terminated string in from user memory.
pub fn cstring(ptr: u64) -> Result<String, i64> {
    if ptr == 0 {
        return Err(errno::EFAULT);
    }
    let mut bytes = Vec::new();
    for i in 0..PATH_MAX as u64 {
        let b: u8 = get(ptr + i)?;
        if b == 0 {
            return String::from_utf8(bytes).map_err(|_| errno::EINVAL);
        }
        bytes.push(b);
    }
    Err(errno::ENAMETOOLONG)
}
