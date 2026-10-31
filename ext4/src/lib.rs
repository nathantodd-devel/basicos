//! `basicos-ext4` — the ext4 filesystem (also ext2/ext3 images, read-only
//! for the block-mapped files those use), as a `#![no_std]` + `alloc`
//! crate over [`basicos_fs::BlockDevice`], so it runs in the kernel and is
//! tested on the host against volumes made by the real `mkfs.ext4` and
//! checked with the real `e2fsck`.
//!
//! What it does:
//!
//! * mounts (and, if the journal says a crash left work undone, first
//!   **replays the jbd2 journal**), verifying the superblock and group
//!   descriptor checksums;
//! * reads everything a Linux-made volume contains: extent trees of any
//!   depth, legacy block maps, sparse files and unwritten extents, long
//!   names, symlinks, hash-tree directories, 64-bit block numbers,
//!   `flex_bg`, `uninit_bg`/`metadata_csum`;
//! * writes: create/overwrite/append/truncate files, `mkdir`/`rmdir`,
//!   `unlink`, `rename`, hard links, symlinks, `chmod` — allocating from
//!   the real bitmaps and keeping every counter and checksum consistent
//!   (verified with `e2fsck -fn` in the tests).
//!
//! What it doesn't: it never *writes* the journal — changes go straight to
//! the filesystem, in an order chosen so a crash leaves things `e2fsck`
//! can repair (data, then metadata) but not journal-atomic. Extended
//! attributes are preserved but not exposed. Volumes with `bigalloc`,
//! quotas, `meta_bg` (write only), `inline_data`, encryption, or an
//! external journal are refused (read-only where reading is safe).

#![no_std]

extern crate alloc;

mod alloc_;
mod crc;
mod dir;
mod extent;
mod fs;
mod inode;
mod journal;
mod layout;
mod ops;

pub use fs::{Ext4Fs, MountInfo};
