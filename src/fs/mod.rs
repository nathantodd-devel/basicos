//! Filesystem layer: FAT32 on top of any [`BlockDevice`] — the primary
//! ATA disk, or the in-memory initrd GRUB loads as a multiboot2 module.
//!
//! Exposes two global mount points rather than requiring callers to
//! thread a `Fat32Fs` handle through the kernel: [`with_disk`] for the
//! ATA-backed volume, [`with_initrd`] for the memory-backed one. Mirrors
//! the singleton pattern `mem::frame`/`mem::heap` already use.

pub mod block;
pub mod fat32;

pub use block::{AtaDisk, BlockDevice, BlockError, MemDisk, BLOCK_SIZE};
pub use fat32::{DirEntry, Fat32Error, Fat32Fs};

use crate::sync::Spinlock;
use alloc::boxed::Box;

static DISK_FS: Spinlock<Option<Fat32Fs>> = Spinlock::new(None);
static INITRD_FS: Spinlock<Option<Fat32Fs>> = Spinlock::new(None);

/// Probe the primary ATA drive and mount it as FAT32, if present.
pub fn init_disk() -> Result<(), &'static str> {
    let disk = AtaDisk::probe().ok_or("no ATA drive found on the primary bus")?;
    let fs = Fat32Fs::mount(Box::new(disk)).map_err(|_| "primary disk isn't a FAT32 volume")?;
    *DISK_FS.lock() = Some(fs);
    Ok(())
}

/// Mount an in-memory disk image (the initrd) as FAT32.
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

/// Run `f` with access to the mounted disk filesystem, if any is mounted.
pub fn with_disk<R>(f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    DISK_FS.lock().as_mut().map(f)
}

/// Run `f` with access to the mounted initrd filesystem, if any is mounted.
pub fn with_initrd<R>(f: impl FnOnce(&mut Fat32Fs) -> R) -> Option<R> {
    INITRD_FS.lock().as_mut().map(f)
}
