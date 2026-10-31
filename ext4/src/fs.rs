//! Mounting, raw block I/O, superblock and group descriptors.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use basicos_fs::block::{BlockDevice, BLOCK_SIZE};
use basicos_fs::vfs::{FsError, FsResult};

use crate::crc::{crc16, crc32c};
use crate::extent::Extent;
use crate::inode::Inode;
use crate::layout::*;

/// What a mount found, for logging.
#[derive(Debug, Clone)]
pub struct MountInfo {
    pub label: String,
    pub block_size: usize,
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u32,
    pub free_inodes: u32,
    pub read_only: bool,
    /// Why writes are refused, if they are.
    pub read_only_reason: Option<&'static str>,
    /// Number of committed journal transactions replayed at mount.
    pub replayed_transactions: u32,
    pub has_journal: bool,
    pub metadata_csum: bool,
}

pub struct Ext4Fs {
    pub(crate) dev: Box<dyn BlockDevice>,
    /// The superblock, 1024 raw bytes, kept current in memory.
    pub(crate) sb: Vec<u8>,
    pub(crate) bs: usize,
    pub(crate) sectors_per_block: u64,
    pub(crate) blocks_count: u64,
    pub(crate) first_data_block: u32,
    pub(crate) bpg: u32,
    pub(crate) ipg: u32,
    pub(crate) inode_size: usize,
    pub(crate) first_ino: u32,
    pub(crate) groups: u32,
    pub(crate) desc_size: usize,
    pub(crate) compat: u32,
    pub(crate) incompat: u32,
    pub(crate) ro_compat: u32,
    pub(crate) csum_seed: u32,
    /// Raw group descriptors, `desc_size` bytes each.
    pub(crate) gdt: Vec<Vec<u8>>,
    /// Block number of the first group-descriptor block.
    pub(crate) gdt_block: u64,
    pub(crate) read_only: bool,
    pub(crate) read_only_reason: Option<&'static str>,
    pub(crate) cache: BTreeMap<u64, Vec<u8>>,
    pub(crate) ext_cache: BTreeMap<u32, Vec<Extent>>,
    /// Inodes held open by callers (streaming handles): freeing an
    /// unlinked inode is deferred until the last one is released.
    pub(crate) pinned: BTreeMap<u32, u32>,
    /// Unlinked (links == 0) inodes waiting for their last close.
    pub(crate) orphans: Vec<u32>,
    pub(crate) clock: fn() -> u64,
    pub(crate) replayed: u32,
}

const CACHE_LIMIT: usize = 512;

fn default_clock() -> u64 {
    0
}

impl Ext4Fs {
    /// Mount `dev`. If the journal is dirty it is replayed first, which
    /// needs the device to be writable.
    pub fn mount(dev: Box<dyn BlockDevice>) -> FsResult<Ext4Fs> {
        let mut fs = Ext4Fs {
            dev,
            sb: Vec::new(),
            bs: 0,
            sectors_per_block: 0,
            blocks_count: 0,
            first_data_block: 0,
            bpg: 0,
            ipg: 0,
            inode_size: 0,
            first_ino: 11,
            groups: 0,
            desc_size: 32,
            compat: 0,
            incompat: 0,
            ro_compat: 0,
            csum_seed: 0,
            gdt: Vec::new(),
            gdt_block: 0,
            read_only: false,
            read_only_reason: None,
            cache: BTreeMap::new(),
            ext_cache: BTreeMap::new(),
            pinned: BTreeMap::new(),
            orphans: Vec::new(),
            clock: default_clock,
            replayed: 0,
        };
        fs.load_superblock()?;
        if fs.incompat & INCOMPAT_RECOVER != 0 {
            // The journal inode lives in the inode table, so the group
            // descriptors are needed first — but unverified: a crash can
            // leave them stale, and replay may be what repairs them.
            fs.load_group_descriptors(false)?;
            fs.replay_journal()?;
            fs.cache.clear();
            fs.ext_cache.clear();
            fs.load_superblock()?;
        }
        fs.load_group_descriptors(true)?;
        Ok(fs)
    }

    /// Give the filesystem a wall clock (seconds since the epoch) for
    /// timestamps. Without one, timestamps are taken from the
    /// superblock's last-write time.
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = clock;
    }

    pub(crate) fn now(&self) -> u64 {
        let t = (self.clock)();
        if t != 0 {
            t
        } else {
            le32(&self.sb, 0x30) as u64
        }
    }

    pub fn info(&self) -> MountInfo {
        MountInfo {
            label: self.label_string(),
            block_size: self.bs,
            blocks: self.blocks_count,
            free_blocks: self.free_blocks_total(),
            inodes: le32(&self.sb, 0),
            free_inodes: le32(&self.sb, 0x10),
            read_only: self.read_only,
            read_only_reason: self.read_only_reason,
            replayed_transactions: self.replayed,
            has_journal: self.compat & COMPAT_HAS_JOURNAL != 0,
            metadata_csum: self.has_metadata_csum(),
        }
    }

    pub(crate) fn label_string(&self) -> String {
        let raw = &self.sb[0x78..0x88];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(16);
        String::from_utf8_lossy(&raw[..end]).into_owned()
    }

    pub fn has_metadata_csum(&self) -> bool {
        self.ro_compat & RO_COMPAT_METADATA_CSUM != 0
    }

    /// Whether group descriptors carry checksums and the `INODE_UNINIT`/
    /// `BLOCK_UNINIT`/`INODE_ZEROED` flags mean anything (`uninit_bg` or
    /// `metadata_csum`). Without it the flag bits must be left alone.
    pub(crate) fn has_gdt_csum(&self) -> bool {
        self.has_metadata_csum() || self.ro_compat & RO_COMPAT_GDT_CSUM != 0
    }

    pub(crate) fn free_blocks_total(&self) -> u64 {
        let lo = le32(&self.sb, 0x0C) as u64;
        let hi = if self.incompat & INCOMPAT_64BIT != 0 { le32(&self.sb, 0x158) as u64 } else { 0 };
        lo | hi << 32
    }

    // --- raw device access -----------------------------------------------------

    fn sectors(&self, blk: u64) -> u64 {
        blk * self.sectors_per_block
    }

    pub(crate) fn check_blk(&self, blk: u64) -> FsResult<()> {
        if blk >= self.blocks_count {
            Err(FsError::Corrupt)
        } else {
            Ok(())
        }
    }

    /// Read one filesystem block (cached).
    pub(crate) fn read_block(&mut self, blk: u64) -> FsResult<Vec<u8>> {
        if let Some(b) = self.cache.get(&blk) {
            return Ok(b.clone());
        }
        self.check_blk(blk)?;
        let mut buf = alloc::vec![0u8; self.bs];
        self.dev.read_blocks(self.sectors(blk), &mut buf).map_err(|_| FsError::Io)?;
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear(); // write-through, so nothing to flush
        }
        self.cache.insert(blk, buf.clone());
        Ok(buf)
    }

    /// Read `n` consecutive blocks, bypassing the cache (bulk data).
    pub(crate) fn read_blocks(&mut self, blk: u64, n: u64) -> FsResult<Vec<u8>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        self.check_blk(blk)?;
        self.check_blk(blk + n - 1)?;
        let mut buf = alloc::vec![0u8; n as usize * self.bs];
        self.dev.read_blocks(self.sectors(blk), &mut buf).map_err(|_| FsError::Io)?;
        // A cached copy may be newer than the disk only if a write is
        // pending, and writes here go straight through — so it's current.
        Ok(buf)
    }

    /// Write one block (write-through).
    pub(crate) fn write_block(&mut self, blk: u64, data: &[u8]) -> FsResult<()> {
        debug_assert_eq!(data.len(), self.bs);
        self.check_blk(blk)?;
        self.dev.write_blocks(self.sectors(blk), data).map_err(|e| match e {
            basicos_fs::block::BlockError::ReadOnly => FsError::ReadOnly,
            _ => FsError::Io,
        })?;
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear();
        }
        self.cache.insert(blk, data.to_vec());
        Ok(())
    }

    /// Write consecutive blocks (bulk data); drops any cached copies.
    pub(crate) fn write_blocks(&mut self, blk: u64, data: &[u8]) -> FsResult<()> {
        let n = (data.len() / self.bs) as u64;
        if n == 0 {
            return Ok(());
        }
        self.check_blk(blk)?;
        self.check_blk(blk + n - 1)?;
        self.dev.write_blocks(self.sectors(blk), data).map_err(|e| match e {
            basicos_fs::block::BlockError::ReadOnly => FsError::ReadOnly,
            _ => FsError::Io,
        })?;
        for b in blk..blk + n {
            self.cache.remove(&b);
        }
        Ok(())
    }

    pub(crate) fn ensure_writable(&self) -> FsResult<()> {
        if self.read_only {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    // --- superblock -------------------------------------------------------------

    fn read_sb_raw(&mut self) -> FsResult<Vec<u8>> {
        let mut raw = alloc::vec![0u8; SB_SIZE];
        let first = (SB_OFFSET / BLOCK_SIZE) as u64;
        self.dev.read_blocks(first, &mut raw).map_err(|_| FsError::Io)?;
        Ok(raw)
    }

    fn sb_csum(sb: &[u8]) -> u32 {
        crc32c(!0, &sb[..0x3FC])
    }

    fn load_superblock(&mut self) -> FsResult<()> {
        let sb = self.read_sb_raw()?;
        if le16(&sb, 0x38) != EXT4_MAGIC {
            return Err(FsError::Corrupt);
        }
        let compat = le32(&sb, 0x5C);
        let incompat = le32(&sb, 0x60);
        let ro_compat = le32(&sb, 0x64);
        if incompat & !INCOMPAT_SUPPORTED != 0 {
            return Err(FsError::Unsupported);
        }
        if incompat & INCOMPAT_RECOVER != 0 && compat & COMPAT_FAST_COMMIT != 0 {
            // A fast-commit journal area can hold work we can't replay.
            return Err(FsError::Unsupported);
        }
        let log_bs = le32(&sb, 0x18);
        if log_bs > 6 {
            return Err(FsError::Corrupt);
        }
        let bs = 1024usize << log_bs;
        let rev = le32(&sb, 0x4C);
        let inode_size = if rev == 0 { 128 } else { le16(&sb, 0x58) as usize };
        if !(128..=bs).contains(&inode_size) || !inode_size.is_power_of_two() {
            return Err(FsError::Corrupt);
        }
        let desc_size = if incompat & INCOMPAT_64BIT != 0 { le16(&sb, 0xFE) as usize } else { 32 };
        if incompat & INCOMPAT_64BIT != 0 && !(64..=1024).contains(&desc_size) {
            return Err(FsError::Corrupt);
        }
        if ro_compat & RO_COMPAT_METADATA_CSUM != 0 {
            // Verify the superblock checksum (type 1 = crc32c).
            if sb[0x175] != 1 || le32(&sb, 0x3FC) != Self::sb_csum(&sb) {
                return Err(FsError::Corrupt);
            }
        }
        let bpg = le32(&sb, 0x20);
        let ipg = le32(&sb, 0x28);
        if bpg == 0 || ipg == 0 || bpg as usize > bs * 8 || ipg as usize > bs * 8 {
            return Err(FsError::Corrupt);
        }
        let blocks_lo = le32(&sb, 4) as u64;
        let blocks_hi = if incompat & INCOMPAT_64BIT != 0 { le32(&sb, 0x150) as u64 } else { 0 };

        self.bs = bs;
        self.sectors_per_block = (bs / BLOCK_SIZE) as u64;
        self.blocks_count = blocks_lo | blocks_hi << 32;
        self.first_data_block = le32(&sb, 0x14);
        self.bpg = bpg;
        self.ipg = ipg;
        self.inode_size = inode_size;
        self.first_ino = if rev == 0 { 11 } else { le32(&sb, 0x54) };
        self.desc_size = desc_size;
        self.compat = compat;
        self.incompat = incompat;
        self.ro_compat = ro_compat;
        self.csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            le32(&sb, 0x270)
        } else {
            crc32c(!0, &sb[0x68..0x78])
        };
        let data_blocks = self.blocks_count.saturating_sub(self.first_data_block as u64);
        self.groups = data_blocks.div_ceil(bpg as u64) as u32;
        if self.groups == 0 || (self.blocks_count * self.sectors_per_block) > self.dev.block_count() {
            return Err(FsError::Corrupt); // device smaller than the filesystem
        }
        self.sb = sb;

        // Decide whether we may write.
        self.read_only = false;
        self.read_only_reason = None;
        let mut refuse = |me: &mut Self, why: &'static str| {
            me.read_only = true;
            me.read_only_reason.get_or_insert(why);
        };
        if ro_compat & RO_COMPAT_FORCES_READONLY != 0 {
            refuse(self, "uses a feature (bigalloc/quota/project/readonly/...) this driver can't update");
        }
        if ro_compat & !RO_COMPAT_KNOWN != 0 {
            refuse(self, "has ro_compat features this driver doesn't know (e.g. orphan_file entries)");
        }
        if incompat & INCOMPAT_EXTENTS == 0 {
            refuse(self, "no extents feature (ext2/ext3 layout): read-only");
        }
        if incompat & INCOMPAT_META_BG != 0 {
            refuse(self, "meta_bg group descriptor layout: read-only");
        }
        if self.groups > 1 && (le32(&self.sb, 0x24) != bpg) {
            refuse(self, "unexpected cluster size");
        }
        Ok(())
    }

    // --- group descriptors -------------------------------------------------------

    pub(crate) fn gd_csum(&self, g: u32, desc: &[u8]) -> u16 {
        if self.has_metadata_csum() {
            let mut crc = crc32c(self.csum_seed, &g.to_le_bytes());
            crc = crc32c(crc, &desc[..0x1E]);
            crc = crc32c(crc, &[0, 0]);
            if self.desc_size > 0x20 {
                crc = crc32c(crc, &desc[0x20..self.desc_size]);
            }
            crc as u16
        } else {
            let mut crc = crc16(!0, &self.sb[0x68..0x78]);
            crc = crc16(crc, &g.to_le_bytes());
            crc = crc16(crc, &desc[..0x1E]);
            if self.desc_size > 0x20 {
                crc = crc16(crc, &desc[0x20..self.desc_size]);
            }
            crc
        }
    }

    fn load_group_descriptors(&mut self, verify: bool) -> FsResult<()> {
        let per_block = self.bs / self.desc_size;
        let gd_blocks = (self.groups as usize).div_ceil(per_block);
        self.gdt_block = self.first_data_block as u64 + 1;
        let raw = self.read_blocks(self.gdt_block, gd_blocks as u64)?;
        self.gdt.clear();
        for g in 0..self.groups as usize {
            let d = raw[g * self.desc_size..(g + 1) * self.desc_size].to_vec();
            if verify && self.has_gdt_csum() {
                let stored = le16(&d, 0x1E);
                // A never-initialised (all-zero, checksum 0) descriptor
                // shouldn't occur; anything else must match.
                if stored != self.gd_csum(g as u32, &d) {
                    return Err(FsError::Corrupt);
                }
            }
            self.gdt.push(d);
        }
        Ok(())
    }

    fn gd_hi(&self, g: u32, lo: usize, hi: usize) -> u64 {
        let d = &self.gdt[g as usize];
        let l = le32(d, lo) as u64;
        if self.desc_size >= 64 { l | (le32(d, hi) as u64) << 32 } else { l }
    }
    fn gd_hi16(&self, g: u32, lo: usize, hi: usize) -> u32 {
        let d = &self.gdt[g as usize];
        let l = le16(d, lo) as u32;
        if self.desc_size >= 64 { l | (le16(d, hi) as u32) << 16 } else { l }
    }
    fn gd_set_hi16(&mut self, g: u32, lo: usize, hi: usize, v: u32) {
        let big = self.desc_size >= 64;
        let d = &mut self.gdt[g as usize];
        put16(d, lo, v as u16);
        if big {
            put16(d, hi, (v >> 16) as u16);
        }
    }

    pub(crate) fn gd_block_bitmap(&self, g: u32) -> u64 {
        self.gd_hi(g, 0x00, 0x20)
    }
    pub(crate) fn gd_inode_bitmap(&self, g: u32) -> u64 {
        self.gd_hi(g, 0x04, 0x24)
    }
    pub(crate) fn gd_inode_table(&self, g: u32) -> u64 {
        self.gd_hi(g, 0x08, 0x28)
    }
    pub(crate) fn gd_free_blocks(&self, g: u32) -> u32 {
        self.gd_hi16(g, 0x0C, 0x2C)
    }
    pub(crate) fn gd_free_inodes(&self, g: u32) -> u32 {
        self.gd_hi16(g, 0x0E, 0x2E)
    }
    pub(crate) fn gd_used_dirs(&self, g: u32) -> u32 {
        self.gd_hi16(g, 0x10, 0x30)
    }
    pub(crate) fn gd_itable_unused(&self, g: u32) -> u32 {
        self.gd_hi16(g, 0x1C, 0x32)
    }
    pub(crate) fn gd_flags(&self, g: u32) -> u16 {
        if self.has_gdt_csum() { le16(&self.gdt[g as usize], 0x12) } else { 0 }
    }
    pub(crate) fn gd_set_free_blocks(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x0C, 0x2C, v)
    }
    pub(crate) fn gd_set_free_inodes(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x0E, 0x2E, v)
    }
    pub(crate) fn gd_set_used_dirs(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x10, 0x30, v)
    }
    pub(crate) fn gd_set_itable_unused(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x1C, 0x32, v)
    }
    pub(crate) fn gd_set_flags(&mut self, g: u32, v: u16) {
        if self.has_gdt_csum() {
            put16(&mut self.gdt[g as usize], 0x12, v);
        }
    }
    pub(crate) fn gd_set_block_bitmap_csum(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x18, 0x38, v)
    }
    pub(crate) fn gd_set_inode_bitmap_csum(&mut self, g: u32, v: u32) {
        self.gd_set_hi16(g, 0x1A, 0x3A, v)
    }

    /// Recompute group `g`'s descriptor checksum and write its block.
    pub(crate) fn flush_gd(&mut self, g: u32) -> FsResult<()> {
        let csum = if self.has_gdt_csum() { Some(self.gd_csum(g, &self.gdt[g as usize].clone())) } else { None };
        if let Some(c) = csum {
            put16(&mut self.gdt[g as usize], 0x1E, c);
        }
        // Only the descriptors that share a block with `g` change, so
        // patch that one block rather than rewriting the table.
        let per_block = self.bs / self.desc_size;
        let blk_idx = g as usize / per_block;
        let blk = self.gdt_block + blk_idx as u64;
        let mut data = self.read_block(blk)?;
        let first = blk_idx * per_block;
        for i in 0..per_block {
            let gi = first + i;
            if gi >= self.groups as usize {
                break;
            }
            data[i * self.desc_size..(i + 1) * self.desc_size].copy_from_slice(&self.gdt[gi]);
        }
        self.write_block(blk, &data)
    }

    /// Write the in-memory superblock: refreshed counters, timestamp,
    /// checksum.
    pub(crate) fn flush_sb(&mut self) -> FsResult<()> {
        // Free counts are the sum over groups (kept exact by every
        // allocation/free), so the superblock never disagrees with them.
        let mut fb = 0u64;
        let mut fi = 0u64;
        for g in 0..self.groups {
            fb += self.gd_free_blocks(g) as u64;
            fi += self.gd_free_inodes(g) as u64;
        }
        put32(&mut self.sb, 0x0C, fb as u32);
        if self.incompat & INCOMPAT_64BIT != 0 {
            put32(&mut self.sb, 0x158, (fb >> 32) as u32);
        }
        put32(&mut self.sb, 0x10, fi as u32);
        let now = self.now();
        if now != 0 {
            put32(&mut self.sb, 0x30, now as u32);
        }
        if self.has_metadata_csum() {
            let c = Self::sb_csum(&self.sb);
            put32(&mut self.sb, 0x3FC, c);
        }
        let first = (SB_OFFSET / BLOCK_SIZE) as u64;
        self.dev.write_blocks(first, &self.sb).map_err(|e| match e {
            basicos_fs::block::BlockError::ReadOnly => FsError::ReadOnly,
            _ => FsError::Io,
        })?;
        self.cache.clear(); // block 0/1 may hold a stale copy of the sb
        Ok(())
    }

    // --- inodes -------------------------------------------------------------------

    pub(crate) fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        crc32c(crc32c(self.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    fn inode_csum(&self, ino: u32, raw: &[u8]) -> u32 {
        let generation = le32(raw, 0x64);
        let mut copy = raw.to_vec();
        copy[0x7C..0x7E].fill(0);
        let has_hi = raw.len() > 128 && le16(raw, 0x80) >= 4;
        if has_hi {
            copy[0x82..0x84].fill(0);
        }
        crc32c(self.inode_seed(ino, generation), &copy)
    }

    /// (block, offset within it) of inode `ino`'s record.
    fn inode_location(&self, ino: u32) -> FsResult<(u64, usize)> {
        if ino == 0 || ino > le32(&self.sb, 0) {
            return Err(FsError::Corrupt);
        }
        let g = (ino - 1) / self.ipg;
        let idx = (ino - 1) % self.ipg;
        let byte = idx as u64 * self.inode_size as u64;
        let blk = self.gd_inode_table(g) + byte / self.bs as u64;
        Ok((blk, (byte % self.bs as u64) as usize))
    }

    pub(crate) fn load_inode(&mut self, ino: u32) -> FsResult<Inode> {
        let (blk, off) = self.inode_location(ino)?;
        let data = self.read_block(blk)?;
        let raw = data[off..off + self.inode_size].to_vec();
        if self.has_metadata_csum() && raw.iter().any(|&b| b != 0) {
            let want = self.inode_csum(ino, &raw);
            let lo = le16(&raw, 0x7C) as u32;
            let hi = if raw.len() > 128 && le16(&raw, 0x80) >= 4 { (le16(&raw, 0x82) as u32) << 16 } else { 0 };
            let has_hi = raw.len() > 128 && le16(&raw, 0x80) >= 4;
            let ok = if has_hi { (lo | hi) == want } else { lo == (want & 0xFFFF) };
            if !ok {
                return Err(FsError::Corrupt);
            }
        }
        Ok(Inode { ino, raw })
    }

    pub(crate) fn store_inode(&mut self, inode: &Inode) -> FsResult<()> {
        self.ensure_writable()?;
        let (blk, off) = self.inode_location(inode.ino)?;
        let mut raw = inode.raw.clone();
        if self.has_metadata_csum() {
            let c = self.inode_csum(inode.ino, &raw);
            put16(&mut raw, 0x7C, c as u16);
            if raw.len() > 128 && le16(&raw, 0x80) >= 4 {
                put16(&mut raw, 0x82, (c >> 16) as u16);
            }
        }
        let mut data = self.read_block(blk)?;
        data[off..off + self.inode_size].copy_from_slice(&raw);
        self.write_block(blk, &data)?;
        self.ext_cache.remove(&inode.ino);
        Ok(())
    }
}
