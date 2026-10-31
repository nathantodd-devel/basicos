//! Filesystem layer: every mounted volume under a DOS-style drive letter.
//!
//! * `A:` the writable in-memory FAT32 ramdisk,
//! * `B:` the FAT32 initrd GRUB loads as a multiboot2 module (read-only),
//! * `C:` onward — one letter per volume found on the machine's ATA disks:
//!   each drive (primary/secondary channel × master/slave) is scanned for a
//!   partition table (MBR with extended partitions, or GPT; a disk with
//!   none is one whole-disk volume) and every partition holding a
//!   filesystem this kernel knows — **FAT32 or ext4** — is mounted, in
//!   discovery order, on the next free letter.
//!
//! The formats themselves live in the `basicos-fs` (FAT32, partitions,
//! the [`FileSystem`] trait) and `basicos-ext4` crates; this module is
//! the kernel-specific glue: the hardware [`AtaDisk`], the mount table
//! ([`with_drive`]), and the drive-letter path rules ([`resolve_path`]).

mod ata_disk;

pub use ata_disk::{AtaDisk, WholeDisk};
pub use basicos_fs::{
    BlockDevice, BlockError, DirEntry, Fat32Error, Fat32Fs, FileSystem, FsError, MemDisk, RamDisk,
    BLOCK_SIZE,
};

use crate::sync::Spinlock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use basicos_fs::partition::{self, Partition, SharedDisk};

/// A drive letter, `A:` (0) to `Z:` (25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drive(pub u8);

#[allow(non_upper_case_globals)]
impl Drive {
    /// `A:` — the writable ramdisk.
    pub const Ramdisk: Drive = Drive(0);
    /// `B:` — the read-only initrd.
    pub const Initrd: Drive = Drive(1);
    /// `C:` — the first disk volume (where `/` lives by default).
    pub const Disk: Drive = Drive(2);

    pub fn letter(self) -> char {
        (b'A' + self.0) as char
    }

    pub fn from_letter(c: u8) -> Option<Drive> {
        let c = c.to_ascii_uppercase();
        c.is_ascii_uppercase().then(|| Drive(c - b'A'))
    }
}

struct Volume {
    fs: Box<dyn FileSystem>,
    /// Where it came from, for `drives`: `"ramdisk"`, `"initrd"`,
    /// `"primary master p1"`, ...
    source: String,
}

const LETTERS: usize = 26;
static VOLUMES: Spinlock<[Option<Volume>; LETTERS]> = Spinlock::new([const { None }; LETTERS]);

fn mount_on(drive: Drive, fs: Box<dyn FileSystem>, source: String) {
    VOLUMES.lock()[drive.0 as usize] = Some(Volume { fs, source });
}

/// Mount an in-memory disk image (the initrd) as FAT32. `B:`.
///
/// # Safety
/// Same as [`crate::mem::multiboot2::Module::as_slice`]: `data` must
/// point at memory the frame allocator has reserved for the life of the
/// kernel.
pub fn init_initrd(data: &'static [u8]) -> Result<(), &'static str> {
    let fs = Fat32Fs::mount(Box::new(MemDisk::new(data))).map_err(|_| "initrd module isn't a FAT32 image")?;
    mount_on(Drive::Initrd, Box::new(fs), String::from("initrd"));
    Ok(())
}

/// Format and mount a fresh, empty, writable in-memory disk. `A:`.
pub fn init_ramdisk() -> Result<(), &'static str> {
    let fs = Fat32Fs::mount(Box::new(RamDisk::new())).map_err(|_| "ramdisk didn't mount its own format")?;
    mount_on(Drive::Ramdisk, Box::new(fs), String::from("ramdisk"));
    Ok(())
}

/// Try FAT32, then ext4, on a fresh handle to the same region (a failed
/// mount consumes its device, so `make` is called once per attempt).
fn probe_filesystem(mut make: impl FnMut() -> Box<dyn BlockDevice>) -> Result<Box<dyn FileSystem>, String> {
    // ext4's magic is at byte 1080; look before committing to a driver so
    // the error we report is the relevant one.
    let mut dev = make();
    let mut sect = [0u8; BLOCK_SIZE];
    let is_ext = dev.read_block(2, &mut sect).is_ok() && sect[0x38] == 0x53 && sect[0x39] == 0xEF;
    drop(dev);
    if is_ext {
        let mut fs = basicos_ext4::Ext4Fs::mount(make()).map_err(|e| alloc::format!("ext4: {:?}", e))?;
        fs.set_clock(|| crate::drivers::rtc::wall_seconds());
        return Ok(Box::new(fs));
    }
    match Fat32Fs::mount(make()) {
        Ok(fs) => Ok(Box::new(fs)),
        Err(_) => Err(String::from("no FAT32 or ext4 filesystem")),
    }
}

/// Result of mounting one volume, for the boot log.
pub struct Mounted {
    pub drive: Drive,
    pub fs_type: &'static str,
    pub label: String,
    pub source: String,
    pub bytes: u64,
    pub read_only: bool,
    pub note: Option<String>,
}

/// Probe every ATA disk and mount each volume found on the next free
/// letter starting at `C:`. Returns a report line per drive/partition
/// (`Ok` mounted, `Err` why not) for the caller to log.
pub fn mount_disks() -> Vec<Result<Mounted, String>> {
    use crate::drivers::ata;
    let mut report = Vec::new();
    let mut next_letter = Drive::Disk.0;
    for drive in ata::probe_all() {
        let disk = Arc::new(AtaDisk::new(drive));
        let (position, model, bytes) = disk.describe();
        let mut whole = WholeDisk(disk.clone());
        let parts = partition::scan(&mut whole);
        if parts.is_empty() {
            report.push(Err(alloc::format!(
                "{} ({}, {} MiB): no partition table or filesystem",
                position, model, bytes >> 20
            )));
            continue;
        }
        for part in parts {
            let source = if part.number == 0 {
                String::from(position)
            } else {
                alloc::format!("{} p{}", position, part.number)
            };
            let make = || -> Box<dyn BlockDevice> {
                if part.number == 0 {
                    Box::new(WholeDisk(disk.clone()))
                } else {
                    Box::new(Partition::new(disk.clone() as Arc<dyn SharedDisk>, part))
                }
            };
            match probe_filesystem(make) {
                Ok(fs) => {
                    if next_letter as usize >= LETTERS {
                        report.push(Err(alloc::format!("{}: out of drive letters", source)));
                        continue;
                    }
                    let drive = Drive(next_letter);
                    next_letter += 1;
                    let m = Mounted {
                        drive,
                        fs_type: fs.fs_type(),
                        label: fs.label(),
                        source: source.clone(),
                        bytes: part.sectors * BLOCK_SIZE as u64,
                        read_only: fs.read_only(),
                        note: fs.note(),
                    };
                    mount_on(drive, fs, source);
                    report.push(Ok(m));
                }
                Err(why) => report.push(Err(alloc::format!("{}: {}", source, why))),
            }
        }
    }
    report
}

/// One mounted volume, for `drives`.
pub struct DriveInfo {
    pub drive: Drive,
    pub fs_type: &'static str,
    pub label: String,
    pub source: String,
    pub read_only: bool,
    pub total: u64,
    pub free: u64,
}

pub fn drives() -> Vec<DriveInfo> {
    let mut out = Vec::new();
    let mut vols = VOLUMES.lock();
    for (i, slot) in vols.iter_mut().enumerate() {
        if let Some(v) = slot {
            let (total, free) = v.fs.space();
            out.push(DriveInfo {
                drive: Drive(i as u8),
                fs_type: v.fs.fs_type(),
                label: v.fs.label(),
                source: v.source.clone(),
                read_only: v.fs.read_only(),
                total,
                free,
            });
        }
    }
    out
}

/// Recognizes a DOS-style two-character prefix — `A:`, `d:`, `C:/foo`,
/// case-insensitive — for any mounted drive letter; a leading slash right
/// after the colon is optional and stripped either way. Anything else
/// falls back to the older, pre-drive-letter rules kept for
/// compatibility: a bare path (`/foo`) means `C:`, and `/initrd/...` (or
/// exactly `/initrd`) means `B:`.
pub fn resolve_path(path: &str) -> (Drive, &str) {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if let Some(drive) = Drive::from_letter(bytes[0]) {
            if is_mounted(drive) {
                return (drive, path[2..].trim_start_matches('/'));
            }
        }
        // An unmounted letter falls through to the no-drive-letter rules
        // below, which will treat the whole string (colon included) as a
        // literal — it simply won't be found, rather than silently
        // picking some other drive.
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

pub fn is_mounted(drive: Drive) -> bool {
    VOLUMES.lock()[drive.0 as usize].is_some()
}

/// Run `f` with the filesystem mounted on `drive`, if it's mounted.
pub fn with_drive<R>(drive: Drive, f: impl FnOnce(&mut dyn FileSystem) -> R) -> Option<R> {
    let mut vols = VOLUMES.lock();
    vols[drive.0 as usize].as_mut().map(|v| f(v.fs.as_mut()))
}

/// Flush every volume's cached state to its device.
pub fn sync_all() {
    let mut vols = VOLUMES.lock();
    for v in vols.iter_mut().flatten() {
        let _ = v.fs.sync();
    }
}
