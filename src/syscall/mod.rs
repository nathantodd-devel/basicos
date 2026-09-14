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

#[cfg(feature = "storage")]
use crate::{fs, sync::Spinlock};
use crate::{pr_debug, pr_info};
#[cfg(feature = "storage")]
use alloc::format;
#[cfg(feature = "storage")]
use alloc::string::String;
#[cfg(feature = "storage")]
use alloc::vec::Vec;

// ============================================================================
// Syscall Numbers (Linux-compatible where applicable)
// ============================================================================

pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const EXIT: u64 = 60;
    pub const GETPID: u64 = 39;
    pub const YIELD: u64 = 24; // sched_yield
                               // Only meaningful with the `storage` feature (see `sys_open` etc.),
                               // but these are just numbers — matching real syscall ABI numbers is
                               // useful documentation even when the handler isn't compiled in, and
                               // calling one with `storage` off just falls through to `dispatch`'s
                               // "unknown syscall" arm (ENOSYS) rather than failing to compile.
    pub const LSEEK: u64 = 8;
    pub const GETDENTS64: u64 = 217; // matches Linux x86_64

    // basicos-specific syscalls (start at 1000 to avoid Linux conflicts)
    pub const KLOG: u64 = 1000; // Write to kernel log
    pub const TREE: u64 = 1001; // Recursively list a mounted filesystem to the kernel log
}

/// `TREE` syscall targets (arg1).
pub mod tree_target {
    pub const DISK: u64 = 0;
    pub const INITRD: u64 = 1;
}

/// `lseek()` whence values (arg3).
pub mod whence {
    pub const SEEK_SET: u64 = 0;
    pub const SEEK_CUR: u64 = 1;
    pub const SEEK_END: u64 = 2;
}

/// `open()` flags (arg2) — same bit values as Linux x86_64, so real C
/// programs' `<fcntl.h>` constants would just work if we ever link one.
pub mod open_flags {
    pub const O_RDONLY: u64 = 0o0;
    pub const O_WRONLY: u64 = 0o1;
    #[allow(dead_code)]
    pub const O_RDWR: u64 = 0o2;
    #[allow(dead_code)]
    pub const O_ACCMODE: u64 = 0o3;
    pub const O_CREAT: u64 = 0o100;
    pub const O_TRUNC: u64 = 0o1000;
    #[allow(dead_code)]
    pub const O_APPEND: u64 = 0o2000;
    pub const O_DIRECTORY: u64 = 0o200000;
}

// ============================================================================
// Error Codes (negated in return value)
// ============================================================================

pub mod errno {
    pub const ENOENT: i64 = -2; // No such file or directory
    pub const EIO: i64 = -5; // I/O error
    pub const EBADF: i64 = -9; // Bad file descriptor
    pub const EFAULT: i64 = -14; // Bad address
    pub const ENODEV: i64 = -19; // No such device (filesystem not mounted)
    pub const ENOTDIR: i64 = -20; // Not a directory
    pub const EISDIR: i64 = -21; // Is a directory
    pub const EINVAL: i64 = -22; // Invalid argument
    pub const EMFILE: i64 = -24; // Too many open files
    pub const ENOSYS: i64 = -38; // Function not implemented
}

// ============================================================================
// File Descriptors
// ============================================================================

const STDIN: u64 = 0;
const STDOUT: u64 = 1;
const STDERR: u64 = 2;

/// Which mounted filesystem a path refers to. There's no real VFS/mount
/// table yet, so this is the whole namespace: `/initrd/...` (or exactly
/// `/initrd`) means the initrd image, everything else means the disk.
#[cfg(feature = "storage")]
#[derive(Debug, Clone, Copy)]
enum FsTarget {
    Disk,
    Initrd,
}

#[cfg(feature = "storage")]
fn with_fs<R>(target: FsTarget, f: impl FnOnce(&mut fs::Fat32Fs) -> R) -> Option<R> {
    match target {
        FsTarget::Disk => fs::with_disk(f),
        FsTarget::Initrd => fs::with_initrd(f),
    }
}

/// Split a syscall path into which filesystem it targets and the
/// remaining path within that filesystem (no leading `/`).
#[cfg(feature = "storage")]
fn split_target(path: &str) -> (FsTarget, String) {
    let trimmed = path.trim_start_matches('/');
    if trimmed == "initrd" {
        (FsTarget::Initrd, String::new())
    } else if let Some(rest) = trimmed.strip_prefix("initrd/") {
        (FsTarget::Initrd, String::from(rest))
    } else {
        (FsTarget::Disk, String::from(trimmed))
    }
}

/// A process's open file or directory. Files are read/written entirely
/// in memory and only touch the FAT32 driver at `open()` (read) and
/// `close()` (write-back, if anything was written) — there's no
/// incremental on-disk update, which keeps `sys_write` from having to
/// know anything about clusters.
#[cfg(feature = "storage")]
enum OpenFile {
    File {
        target: FsTarget,
        path: String,
        data: Vec<u8>,
        pos: usize,
        dirty: bool,
    },
    Dir {
        entries: Vec<fs::DirEntry>,
        pos: usize,
    },
}

#[cfg(feature = "storage")]
const MAX_OPEN_FILES: usize = 16;
/// fds 0-2 are stdin/stdout/stderr; real files start here.
#[cfg(feature = "storage")]
const FD_BASE: u64 = 3;

#[cfg(feature = "storage")]
static OPEN_FILES: Spinlock<[Option<OpenFile>; MAX_OPEN_FILES]> =
    Spinlock::new([const { None }; MAX_OPEN_FILES]);

#[cfg(feature = "storage")]
fn fd_index(fd: u64) -> Option<usize> {
    let idx = fd.checked_sub(FD_BASE)? as usize;
    (idx < MAX_OPEN_FILES).then_some(idx)
}

#[cfg(feature = "storage")]
fn alloc_fd(file: OpenFile) -> Option<u64> {
    let mut table = OPEN_FILES.lock();
    let (idx, slot) = table.iter_mut().enumerate().find(|(_, s)| s.is_none())?;
    *slot = Some(file);
    Some(idx as u64 + FD_BASE)
}

/// Copy a NUL-terminated path string out of user memory.
///
/// Same trust level as the rest of this syscall layer (see `sys_write`):
/// no page-table validation, just a bounded scan. Fine for a single
/// trusted test program; not fine for an untrusted one.
#[cfg(feature = "storage")]
fn read_user_cstring(ptr: u64) -> Option<String> {
    if ptr == 0 {
        return None;
    }
    const MAX_PATH: usize = 256;
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, MAX_PATH) };
    let len = bytes.iter().position(|&b| b == 0)?;
    core::str::from_utf8(&bytes[..len]).ok().map(String::from)
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
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
) -> i64 {
    match number {
        nr::READ => sys_read(arg1, arg2, arg3),
        nr::WRITE => sys_write(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::OPEN => sys_open(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::CLOSE => sys_close(arg1),
        nr::EXIT => sys_exit(arg1 as i32),
        nr::GETPID => sys_getpid(),
        nr::YIELD => sys_yield(),
        #[cfg(feature = "storage")]
        nr::LSEEK => sys_lseek(arg1, arg2, arg3),
        #[cfg(feature = "storage")]
        nr::GETDENTS64 => sys_getdents64(arg1, arg2, arg3),
        nr::KLOG => sys_klog(arg1, arg2),
        #[cfg(feature = "storage")]
        nr::TREE => sys_tree(arg1),
        _ => {
            pr_debug!("Unknown syscall: {}", number);
            errno::ENOSYS
        }
    }
}

// ============================================================================
// Syscall Implementations
// ============================================================================

/// read(fd, buf, count) - Read from a file descriptor.
fn sys_read(fd: u64, buf: u64, count: u64) -> i64 {
    match fd {
        STDIN => {
            // Would read from keyboard buffer
            pr_debug!("sys_read: stdin not implemented");
            0 // EOF for now
        }
        STDOUT | STDERR => errno::EBADF, // write-only
        #[cfg(feature = "storage")]
        _ => {
            let Some(idx) = fd_index(fd) else {
                return errno::EBADF;
            };
            let mut table = OPEN_FILES.lock();
            match table[idx].as_mut() {
                Some(OpenFile::File { data, pos, .. }) => {
                    let n = (count as usize).min(data.len().saturating_sub(*pos));
                    if n == 0 {
                        return 0; // EOF
                    }
                    if buf == 0 {
                        return errno::EFAULT;
                    }
                    // Safety: same trust level as sys_write below.
                    let dest = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n) };
                    dest.copy_from_slice(&data[*pos..*pos + n]);
                    *pos += n;
                    n as i64
                }
                Some(OpenFile::Dir { .. }) => errno::EISDIR,
                None => errno::EBADF,
            }
        }
        #[cfg(not(feature = "storage"))]
        _ => errno::EBADF, // no file table without `storage`
    }
}

/// write(fd, buf, count) - Write to a file descriptor.
fn sys_write(fd: u64, buf: u64, count: u64) -> i64 {
    match fd {
        STDOUT | STDERR => {
            // Validate pointer (basic check - just ensure it's in user space)
            // In a real kernel, we'd check against the process's memory map
            if buf == 0 {
                return errno::EFAULT;
            }

            // Safety: We're trusting the user pointer here. In a real kernel,
            // we'd use copy_from_user() with proper validation.
            let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count as usize) };

            // Convert to string and print
            if let Ok(s) = core::str::from_utf8(slice) {
                // Write to VGA
                crate::drivers::vga::print(s);
                // Also to serial
                crate::drivers::serial::write_str(s);
                count as i64
            } else {
                // Not valid UTF-8, write byte by byte
                for &byte in slice {
                    crate::drivers::vga::print_char(byte);
                    crate::drivers::serial::write_byte(byte);
                }
                count as i64
            }
        }
        #[cfg(feature = "storage")]
        _ => {
            let Some(idx) = fd_index(fd) else {
                return errno::EBADF;
            };
            if buf == 0 {
                return errno::EFAULT;
            }
            // Safety: trusting the user pointer, same as everywhere else
            // in this syscall layer — see the STDOUT/STDERR arm above.
            let src = unsafe { core::slice::from_raw_parts(buf as *const u8, count as usize) };
            let mut table = OPEN_FILES.lock();
            match table[idx].as_mut() {
                Some(OpenFile::File {
                    data, pos, dirty, ..
                }) => {
                    let end = *pos + src.len();
                    if data.len() < end {
                        data.resize(end, 0);
                    }
                    data[*pos..end].copy_from_slice(src);
                    *pos = end;
                    *dirty = true;
                    src.len() as i64
                }
                Some(OpenFile::Dir { .. }) => errno::EISDIR,
                None => errno::EBADF,
            }
        }
        #[cfg(not(feature = "storage"))]
        _ => errno::EBADF, // no file table without `storage`
    }
}

/// open(path, flags, mode) - Open (optionally creating) a file or
/// directory on one of the mounted FAT32 filesystems, returning a new
/// fd. `path` is resolved by `split_target`: `/initrd/...` for the
/// initrd, anything else for the disk.
#[cfg(feature = "storage")]
fn sys_open(path_ptr: u64, flags: u64, _mode: u64) -> i64 {
    use open_flags::*;

    let Some(path) = read_user_cstring(path_ptr) else {
        return errno::EFAULT;
    };
    let (target, rel_path) = split_target(&path);
    let creating = flags & O_CREAT != 0;
    let truncating = flags & O_TRUNC != 0;

    let opened: Option<Result<OpenFile, i64>> = with_fs(target, |vol| {
        if rel_path.is_empty() {
            return vol
                .read_dir_at("/")
                .map(|entries| OpenFile::Dir { entries, pos: 0 })
                .map_err(|_| errno::EIO);
        }
        match vol.find(&rel_path) {
            Ok(entry) if entry.is_dir => vol
                .read_dir_at(&rel_path)
                .map(|entries| OpenFile::Dir { entries, pos: 0 })
                .map_err(|_| errno::EIO),
            Ok(entry) => {
                let data = if truncating {
                    Vec::new()
                } else {
                    vol.read_file(&entry).map_err(|_| errno::EIO)?
                };
                Ok(OpenFile::File {
                    target,
                    path: rel_path.clone(),
                    data,
                    pos: 0,
                    dirty: truncating,
                })
            }
            Err(fs::Fat32Error::NotFound) if creating => Ok(OpenFile::File {
                target,
                path: rel_path.clone(),
                data: Vec::new(),
                pos: 0,
                dirty: true,
            }),
            Err(fs::Fat32Error::NotFound) => Err(errno::ENOENT),
            Err(_) => Err(errno::EIO),
        }
    });

    match opened {
        None => errno::ENODEV,
        Some(Err(e)) => e,
        Some(Ok(file)) => alloc_fd(file).unwrap_or(errno::EMFILE as u64) as i64,
    }
}

/// close(fd) - Close a file descriptor, flushing any writes to disk.
#[cfg(feature = "storage")]
fn sys_close(fd: u64) -> i64 {
    let Some(idx) = fd_index(fd) else {
        return errno::EBADF;
    };
    let file = OPEN_FILES.lock()[idx].take();
    match file {
        None => errno::EBADF,
        Some(OpenFile::Dir { .. }) => 0,
        Some(OpenFile::File {
            target,
            path,
            data,
            dirty,
            ..
        }) => {
            if !dirty {
                return 0;
            }
            match with_fs(target, |vol| vol.write_file(&path, &data)) {
                Some(Ok(())) => 0,
                Some(Err(_)) => errno::EIO,
                None => errno::ENODEV,
            }
        }
    }
}

/// lseek(fd, offset, whence) - Reposition a file's read/write cursor.
/// `offset` is reinterpreted as signed (matches the Linux ABI, where
/// it's `off_t`); `whence` is one of `whence::{SEEK_SET,SEEK_CUR,SEEK_END}`.
/// Returns the resulting offset, or a negative errno.
#[cfg(feature = "storage")]
fn sys_lseek(fd: u64, offset: u64, whence: u64) -> i64 {
    let Some(idx) = fd_index(fd) else {
        return errno::EBADF;
    };
    let offset = offset as i64;
    let mut table = OPEN_FILES.lock();
    match table[idx].as_mut() {
        Some(OpenFile::File { data, pos, .. }) => {
            let base = match whence {
                whence::SEEK_SET => 0,
                whence::SEEK_CUR => *pos as i64,
                whence::SEEK_END => data.len() as i64,
                _ => return errno::EINVAL,
            };
            match base.checked_add(offset) {
                Some(new_pos) if new_pos >= 0 => {
                    *pos = new_pos as usize;
                    new_pos
                }
                _ => errno::EINVAL,
            }
        }
        Some(OpenFile::Dir { .. }) => errno::EISDIR,
        None => errno::EBADF,
    }
}

/// getdents64(fd, dirp, count) - Read directory entries into `dirp` as
/// packed `linux_dirent64` records, matching glibc's `readdir()` ABI:
///
/// ```c
/// struct linux_dirent64 {
///     ino64_t        d_ino;
///     off64_t        d_off;
///     unsigned short d_reclen;
///     unsigned char  d_type;
///     char           d_name[];  // NUL-terminated
/// };
/// ```
///
/// Fills as many entries as fit in `count` bytes and returns the byte
/// count written (0 once the directory is exhausted), advancing the
/// fd's position — callers loop until it returns 0, same as Linux.
#[cfg(feature = "storage")]
fn sys_getdents64(fd: u64, dirp: u64, count: u64) -> i64 {
    const HEADER_LEN: usize = 19; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;

    let Some(idx) = fd_index(fd) else {
        return errno::EBADF;
    };
    if dirp == 0 {
        return errno::EFAULT;
    }

    let mut table = OPEN_FILES.lock();
    let (entries, pos) = match table[idx].as_mut() {
        Some(OpenFile::Dir { entries, pos }) => (entries, pos),
        Some(OpenFile::File { .. }) => return errno::ENOTDIR,
        None => return errno::EBADF,
    };

    // Safety: same trust level as sys_read/sys_write.
    let out = unsafe { core::slice::from_raw_parts_mut(dirp as *mut u8, count as usize) };
    let mut written = 0usize;

    while *pos < entries.len() {
        let entry = &entries[*pos];
        let name = entry.name.as_bytes();
        // NUL-terminated name, record padded to 8-byte alignment (glibc
        // relies on d_reclen being a multiple of 8 to walk the buffer).
        let reclen = (HEADER_LEN + name.len() + 1 + 7) & !7;
        if written + reclen > out.len() {
            break; // caller's buffer is full; they'll call again
        }

        let rec = &mut out[written..written + reclen];
        rec.fill(0);
        let ino = *pos as u64 + 1; // no real inode numbers, just unique+nonzero
        rec[0..8].copy_from_slice(&ino.to_le_bytes());
        rec[8..16].copy_from_slice(&ino.to_le_bytes()); // d_off (unused by us)
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = if entry.is_dir { DT_DIR } else { DT_REG };
        rec[HEADER_LEN..HEADER_LEN + name.len()].copy_from_slice(name);
        // rec[HEADER_LEN + name.len()] is already 0 (NUL) from fill(0).

        written += reclen;
        *pos += 1;
    }

    written as i64
}

/// exit(status) - Terminate the current process.
fn sys_exit(status: i32) -> i64 {
    pr_info!("Process exited with status: {}", status);

    // For now, just halt. In a real kernel, we'd:
    // 1. Mark the process as zombie
    // 2. Free its resources
    // 3. Notify parent
    // 4. Schedule next process
    loop {
        unsafe {
            core::arch::asm!("cli", "hlt");
        }
    }
}

/// getpid() - Get the current process ID.
fn sys_getpid() -> i64 {
    // TODO: Return actual PID once we have a process table
    1 // Init process
}

/// yield() - Yield the CPU to another process.
fn sys_yield() -> i64 {
    // TODO: Implement once we have a scheduler
    // For now, just return success
    0
}

/// klog(buf, len) - Write directly to kernel log (basicos-specific).
fn sys_klog(buf: u64, len: u64) -> i64 {
    if buf == 0 {
        return errno::EFAULT;
    }

    let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };

    if let Ok(s) = core::str::from_utf8(slice) {
        pr_info!("[user] {}", s);
        len as i64
    } else {
        errno::EINVAL
    }
}

/// tree(target) - Recursively list a mounted FAT32 filesystem to the
/// kernel log (basicos-specific). `target` is `tree_target::DISK` or
/// `tree_target::INITRD`.
///
/// Ring 3 can't walk the filesystem itself — no port I/O, no access to
/// the kernel heap or the `fs::Fat32Fs` handle — so this does the whole
/// walk in kernel mode and just reports success/failure back to userspace.
#[cfg(feature = "storage")]
fn sys_tree(target: u64) -> i64 {
    let (label, result) = match target {
        tree_target::DISK => ("disk", fs::with_disk(|f| print_tree(f, "/", 0))),
        tree_target::INITRD => ("initrd", fs::with_initrd(|f| print_tree(f, "/", 0))),
        _ => return errno::EINVAL,
    };
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
fn print_tree(fs: &mut fs::Fat32Fs, path: &str, depth: usize) -> Result<(), fs::Fat32Error> {
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
