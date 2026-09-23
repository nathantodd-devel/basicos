//! Filesystem layer: FAT32 on top of any [`BlockDevice`] — the primary
//! ATA disk, the in-memory initrd GRUB loads as a multiboot2 module, or
//! the writable in-memory [`RamDisk`].
//!
//! The format itself (parsing, directory walks, the ramdisk's own
//! "mkfs", the [`BlockDevice`] trait) lives in the `basicos-fs` crate
//! now, not here — see its own doc comment for why. What's left in this
//! module is purely kernel-specific glue: [`AtaDisk`] (the one
//! [`BlockDevice`] impl that needs real hardware access), three global
//! mount points ([`with_disk`]/[`with_initrd`]/[`with_ramdisk`], the
//! same singleton pattern `mem::frame`/`mem::heap` use) so callers don't
//! have to thread a `Fat32Fs` handle through the kernel, and on top of
//! that, [`Drive`]/[`resolve_path`]/[`with_drive`] giving the three a
//! single DOS-style namespace — `A:` the ramdisk, `B:` the initrd, `C:`
//! the main disk — so callers (the syscall layer, the shell) don't each
//! need their own copy of "which drive does this path mean". See
//! [`resolve_path`] for the exact rules.

mod ata_disk;

pub use ata_disk::AtaDisk;
pub use basicos_fs::{BlockDevice, BlockError, DirEntry, Fat32Error, Fat32Fs, MemDisk, RamDisk, BLOCK_SIZE};

use crate::sync::Spinlock;
use alloc::boxed::Box;

static DISK_FS: Spinlock<Option<Fat32Fs>> = Spinlock::new(None);
static INITRD_FS: Spinlock<Option<Fat32Fs>> = Spinlock::new(None);
static RAMDISK_FS: Spinlock<Option<Fat32Fs>> = Spinlock::new(None);

/// Probe the primary ATA drive and mount it as FAT32, if present. `C:`.
pub fn init_disk() -> Result<(), &'static str> {
    let disk = AtaDisk::probe().ok_or("no ATA drive found on the primary bus")?;
    let fs = Fat32Fs::mount(Box::new(disk)).map_err(|_| "primary disk isn't a FAT32 volume")?;
    *DISK_FS.lock() = Some(fs);
    Ok(())
}

/// Mount an in-memory disk image (the initrd) as FAT32. `B:`.
///
/// # Safety
/// Same as [`crate::mem::multiboot2::Module::as_slice`]: `data` must
/// point at memory the frame allocator has reserved for the life of the
/// kernel.
pub fn init_initrd(data: &'static [u8]) -> Result<(), &'static str> {
    let disk = MemDisk::new(data);
    let fs = Fat32Fs::mount(Box::new(disk)).map_err(|_| "initrd module isn't a FAT32 image")?;
    *INITRD_FS.lock() = Some(fs);
    Ok(())
}

/// Format and mount a fresh, empty, writable in-memory disk. `A:`.
pub fn init_ramdisk() -> Result<(), &'static str> {
    let disk = RamDisk::new();
    let fs = Fat32Fs::mount(Box::new(disk)).map_err(|_| "ramdisk didn't mount its own format")?;
    *RAMDISK_FS.lock() = Some(fs);
    Ok(())
}

/// Run `f` with access to the mounted disk filesystem, if any is mounted.
pub fn with_disk<R>(f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    DISK_FS.lock().as_mut().map(f)
}

/// Run `f` with access to the mounted initrd filesystem, if any is mounted.
pub fn with_initrd<R>(f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    INITRD_FS.lock().as_mut().map(f)
}

/// Run `f` with access to the mounted ramdisk filesystem, if any is mounted.
pub fn with_ramdisk<R>(f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    RAMDISK_FS.lock().as_mut().map(f)
}

/// A DOS-style drive letter, mapping to one of the three mount points
/// above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// `A:` — the writable ramdisk.
    Ramdisk,
    /// `B:` — the read-only initrd.
    Initrd,
    /// `C:` — the main ATA disk.
    Disk,
}

impl Drive {
    pub fn letter(self) -> char {
        match self {
            Drive::Ramdisk => 'A',
            Drive::Initrd => 'B',
            Drive::Disk => 'C',
        }
    }
}

/// Split a path into which drive it's on and the remaining path within
/// that drive's filesystem (no leading `/`).
///
/// Recognizes a DOS-style two-character prefix — `A:`, `b:`, `C:/foo`,
/// case-insensitive — for any of the three mounted drives; a leading
/// slash right after the colon is optional and stripped either way.
/// Anything else falls back to the older, pre-drive-letter rules kept
/// for compatibility: a bare path (`/foo`) means `C:`, and `/initrd/...`
/// (or exactly `/initrd`) means `B:`, matching what this used to be the
/// *only* way to reach the initrd before drive letters existed.
pub fn resolve_path(path: &str) -> (Drive, &str) {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = match bytes[0].to_ascii_uppercase() {
            b'A' => Some(Drive::Ramdisk),
            b'B' => Some(Drive::Initrd),
            b'C' => Some(Drive::Disk),
            _ => None,
        };
        if let Some(drive) = drive {
            return (drive, path[2..].trim_start_matches('/'));
        }
        // An unrecognized letter (D:, X:, ...) falls through to the
        // no-drive-letter rules below, which will just treat the whole
        // string (colon included) as a literal — harmless, and it'll
        // simply not be found rather than silently picking a drive.
    }

    let trimmed = path.trim_start_matches('/');
    if trimmed == "initrd" {
        (Drive::Initrd, "")
    } else if let Some(rest) = trimmed.strip_prefix("initrd/") {
        (Drive::Initrd, rest)
    } else {
        (Drive::Disk, trimmed)
    }
}

/// Run `f` with the filesystem mounted on `drive`, if it's mounted.
pub fn with_drive<R>(drive: Drive, f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    match drive {
        Drive::Ramdisk => with_ramdisk(f),
        Drive::Initrd => with_initrd(f),
        Drive::Disk => with_disk(f),
    }
}
