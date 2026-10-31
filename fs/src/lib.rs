//! `basicos-fs` — the FAT32 filesystem format basicos speaks, decoupled
//! from the kernel it was originally written inside.
//!
//! `#![no_std]` (plus `alloc`) rather than depending on kernel types
//! directly, specifically so this crate works from two very different
//! contexts:
//!
//! - **In the kernel**, mounted on a [`BlockDevice`] backed by real
//!   hardware (the ATA driver) or a boot-time in-memory image (the
//!   initrd, or the writable [`ramdisk::RamDisk`]).
//! - **In a host-side tool** (a future `mkfs`/`fsck`-equivalent binary,
//!   living outside this crate) — `no_std` code runs under `std` just
//!   fine, so the exact same format logic that mounts a real disk in
//!   the kernel can read/write a `.img` file on the developer's own
//!   machine, the same way `e2fsprogs` and the ext4 kernel driver share
//!   a format without sharing an OS.
//!
//! [`BlockDevice`] is the seam between the two: this crate only ever
//! talks to a disk through that trait, never a concrete device, so it
//! has no idea (and no need to know) whether it's talking to a real ATA
//! bus or a `std::fs::File`.

#![no_std]

extern crate alloc;

pub mod block;
pub mod fat32;
pub mod partition;
pub mod path;
pub mod ramdisk;
pub mod vfs;

pub use block::{BlockDevice, BlockError, MemDisk, BLOCK_SIZE};
pub use fat32::{Fat32Error, Fat32Fs};
pub use vfs::{Attr, DirEntry, FileSystem, FsError, FsResult};
pub use ramdisk::RamDisk;
