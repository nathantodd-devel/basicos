//! An inode as raw bytes with typed accessors. Keeping the raw record
//! (rather than decoding into a struct and re-encoding) means fields this
//! driver doesn't know about — extended attributes in the slack space,
//! nanosecond timestamps, project IDs — survive a read-modify-write.

use crate::layout::*;
use alloc::vec::Vec;

#[derive(Clone)]
pub struct Inode {
    pub ino: u32,
    pub raw: Vec<u8>,
}

impl Inode {
    pub fn zeroed(ino: u32, inode_size: usize) -> Inode {
        Inode { ino, raw: alloc::vec![0u8; inode_size] }
    }

    pub fn mode(&self) -> u16 {
        le16(&self.raw, 0)
    }
    pub fn set_mode(&mut self, m: u16) {
        put16(&mut self.raw, 0, m);
    }
    pub fn is_dir(&self) -> bool {
        self.mode() & S_IFMT == S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        self.mode() & S_IFMT == S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.mode() & S_IFMT == S_IFLNK
    }
    pub fn perm(&self) -> u32 {
        (self.mode() & 0o7777) as u32
    }

    pub fn uid(&self) -> u32 {
        le16(&self.raw, 2) as u32 | (le16(&self.raw, 0x78) as u32) << 16
    }
    pub fn gid(&self) -> u32 {
        le16(&self.raw, 0x18) as u32 | (le16(&self.raw, 0x7A) as u32) << 16
    }
    pub fn set_uid(&mut self, v: u32) {
        put16(&mut self.raw, 2, v as u16);
        put16(&mut self.raw, 0x78, (v >> 16) as u16);
    }
    pub fn set_gid(&mut self, v: u32) {
        put16(&mut self.raw, 0x18, v as u16);
        put16(&mut self.raw, 0x7A, (v >> 16) as u16);
    }

    pub fn size(&self) -> u64 {
        le32(&self.raw, 4) as u64 | (le32(&self.raw, 0x6C) as u64) << 32
    }
    pub fn set_size(&mut self, s: u64) {
        put32(&mut self.raw, 4, s as u32);
        put32(&mut self.raw, 0x6C, (s >> 32) as u32);
    }

    pub fn links(&self) -> u16 {
        le16(&self.raw, 0x1A)
    }
    pub fn set_links(&mut self, n: u16) {
        put16(&mut self.raw, 0x1A, n);
    }

    pub fn flags(&self) -> u32 {
        le32(&self.raw, 0x20)
    }
    pub fn set_flags(&mut self, f: u32) {
        put32(&mut self.raw, 0x20, f);
    }
    pub fn has_flag(&self, f: u32) -> bool {
        self.flags() & f != 0
    }

    pub fn generation(&self) -> u32 {
        le32(&self.raw, 0x64)
    }
    pub fn set_generation(&mut self, g: u32) {
        put32(&mut self.raw, 0x64, g);
    }

    /// The 60-byte `i_block` area: extent root, block map, or a fast
    /// symlink's target.
    pub fn i_block(&self) -> &[u8] {
        &self.raw[0x28..0x28 + 60]
    }
    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[0x28..0x28 + 60]
    }

    /// `i_blocks` in 512-byte units (or fs blocks for `HUGE_FILE` inodes —
    /// callers that need bytes use `Ext4Fs::inode_blocks_512`).
    pub fn blocks_raw(&self) -> u64 {
        le32(&self.raw, 0x1C) as u64 | (le16(&self.raw, 0x74) as u64) << 32
    }
    pub fn set_blocks_raw(&mut self, n: u64) {
        put32(&mut self.raw, 0x1C, n as u32);
        put16(&mut self.raw, 0x74, (n >> 32) as u16);
    }

    pub fn file_acl(&self) -> u64 {
        le32(&self.raw, 0x68) as u64 | (le16(&self.raw, 0x76) as u64) << 32
    }

    pub fn extra_isize(&self) -> usize {
        if self.raw.len() > 128 { le16(&self.raw, 0x80) as usize } else { 0 }
    }

    // --- timestamps: 32-bit seconds + 2 epoch bits in the extra field ---------
    fn time(&self, lo: usize, extra: usize) -> u64 {
        let base = le32(&self.raw, lo) as i32 as i64;
        let hi = if self.extra_isize() >= extra + 4 - 0x80 && self.raw.len() >= extra + 4 {
            (le32(&self.raw, extra) & 3) as i64
        } else {
            0
        };
        (base + (hi << 32)).max(0) as u64
    }
    fn set_time(&mut self, lo: usize, extra: usize, secs: u64) {
        put32(&mut self.raw, lo, secs as u32);
        if self.extra_isize() >= extra + 4 - 0x80 && self.raw.len() >= extra + 4 {
            let e = le32(&self.raw, extra) & !3 | ((secs >> 32) & 3) as u32;
            put32(&mut self.raw, extra, e);
        }
    }
    pub fn atime(&self) -> u64 {
        self.time(8, 0x8C)
    }
    pub fn ctime(&self) -> u64 {
        self.time(0xC, 0x84)
    }
    pub fn mtime(&self) -> u64 {
        self.time(0x10, 0x88)
    }
    pub fn dtime(&self) -> u32 {
        le32(&self.raw, 0x14)
    }
    pub fn set_atime(&mut self, s: u64) {
        self.set_time(8, 0x8C, s)
    }
    pub fn set_ctime(&mut self, s: u64) {
        self.set_time(0xC, 0x84, s)
    }
    pub fn set_mtime(&mut self, s: u64) {
        self.set_time(0x10, 0x88, s)
    }
    pub fn set_dtime(&mut self, s: u32) {
        put32(&mut self.raw, 0x14, s);
    }
    pub fn set_crtime(&mut self, s: u64) {
        if self.raw.len() >= 0x98 && self.extra_isize() >= 0x98 - 0x80 {
            put32(&mut self.raw, 0x90, s as u32);
            put32(&mut self.raw, 0x94, ((s >> 32) & 3) as u32);
        }
    }
}
