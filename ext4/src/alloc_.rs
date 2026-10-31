//! Block and inode bitmaps: allocation, freeing, and the lazy-init
//! (`uninit_bg`) group flags.

use alloc::vec::Vec;
use basicos_fs::vfs::{FsError, FsResult};

use crate::crc::crc32c;
use crate::fs::Ext4Fs;
use crate::layout::*;

fn get_bit(bm: &[u8], i: usize) -> bool {
    bm[i / 8] & (1 << (i % 8)) != 0
}
fn set_bit(bm: &mut [u8], i: usize) {
    bm[i / 8] |= 1 << (i % 8);
}
fn clear_bit(bm: &mut [u8], i: usize) {
    bm[i / 8] &= !(1 << (i % 8));
}

impl Ext4Fs {
    pub(crate) fn group_first_block(&self, g: u32) -> u64 {
        self.first_data_block as u64 + g as u64 * self.bpg as u64
    }

    pub(crate) fn blocks_in_group(&self, g: u32) -> u32 {
        let first = self.group_first_block(g);
        (self.blocks_count - first).min(self.bpg as u64) as u32
    }

    pub(crate) fn group_of_block(&self, blk: u64) -> u32 {
        (blk.saturating_sub(self.first_data_block as u64) / self.bpg as u64) as u32
    }

    /// Does group `g` hold a backup superblock + descriptor table?
    fn has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            let b1 = le32(&self.sb, 0x24C);
            let b2 = le32(&self.sb, 0x250);
            return g == b1 || g == b2;
        }
        if self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 {
            return true;
        }
        if g <= 1 {
            return true;
        }
        if g % 2 == 0 {
            return false;
        }
        for base in [3u32, 5, 7] {
            let mut p = base;
            while p < g {
                p = p.saturating_mul(base);
            }
            if p == g {
                return true;
            }
        }
        false
    }

    fn itable_blocks(&self) -> u64 {
        (self.ipg as u64 * self.inode_size as u64).div_ceil(self.bs as u64)
    }

    /// The bitmap a `BLOCK_UNINIT` group would have if it had been
    /// written: backup sb/descriptors plus the group's own metadata.
    fn synth_block_bitmap(&self, g: u32) -> Vec<u8> {
        let mut bm = alloc::vec![0u8; self.bs];
        let first = self.group_first_block(g);
        let n = self.blocks_in_group(g) as u64;
        let mark = |bm: &mut [u8], blk: u64| {
            if blk >= first && blk < first + n {
                set_bit(bm, (blk - first) as usize);
            }
        };
        if self.has_super(g) {
            let gd_blocks = (self.groups as u64 * self.desc_size as u64).div_ceil(self.bs as u64);
            let reserved = le16(&self.sb, 0xCE) as u64;
            for b in first..first + 1 + gd_blocks + reserved {
                mark(&mut bm, b);
            }
        }
        mark(&mut bm, self.gd_block_bitmap(g));
        mark(&mut bm, self.gd_inode_bitmap(g));
        let it = self.gd_inode_table(g);
        for b in it..it + self.itable_blocks() {
            mark(&mut bm, b);
        }
        for i in n as usize..self.bs * 8 {
            set_bit(&mut bm, i);
        }
        bm
    }

    pub(crate) fn read_block_bitmap(&mut self, g: u32) -> FsResult<Vec<u8>> {
        if self.gd_flags(g) & BG_BLOCK_UNINIT != 0 {
            return Ok(self.synth_block_bitmap(g));
        }
        self.read_block(self.gd_block_bitmap(g))
    }

    fn write_block_bitmap(&mut self, g: u32, mut bm: Vec<u8>) -> FsResult<()> {
        let n = self.blocks_in_group(g) as usize;
        for i in n..self.bs * 8 {
            set_bit(&mut bm, i);
        }
        if self.has_metadata_csum() {
            let c = crc32c(self.csum_seed, &bm[..self.bpg as usize / 8]);
            self.gd_set_block_bitmap_csum(g, c);
        }
        let blk = self.gd_block_bitmap(g);
        self.write_block(blk, &bm)
    }

    pub(crate) fn read_inode_bitmap(&mut self, g: u32) -> FsResult<Vec<u8>> {
        if self.gd_flags(g) & BG_INODE_UNINIT != 0 {
            let mut bm = alloc::vec![0u8; self.bs];
            for i in self.ipg as usize..self.bs * 8 {
                set_bit(&mut bm, i);
            }
            return Ok(bm);
        }
        self.read_block(self.gd_inode_bitmap(g))
    }

    fn write_inode_bitmap(&mut self, g: u32, mut bm: Vec<u8>) -> FsResult<()> {
        for i in self.ipg as usize..self.bs * 8 {
            set_bit(&mut bm, i);
        }
        if self.has_metadata_csum() {
            let c = crc32c(self.csum_seed, &bm[..self.ipg as usize / 8]);
            self.gd_set_inode_bitmap_csum(g, c);
        }
        let blk = self.gd_inode_bitmap(g);
        self.write_block(blk, &bm)
    }

    /// Where to start looking for space for `ino`'s data: its own group.
    pub(crate) fn inode_goal(&self, ino: u32) -> u64 {
        let g = (ino.saturating_sub(1)) / self.ipg;
        self.group_first_block(g.min(self.groups - 1))
    }

    /// Allocate up to `want` contiguous blocks, starting the search at
    /// `goal`. Returns `(first_block, count)` with `1 <= count <= want`.
    pub(crate) fn alloc_run(&mut self, goal: u64, want: u32) -> FsResult<(u64, u32)> {
        self.ensure_writable()?;
        let g0 = if goal < self.first_data_block as u64 || goal >= self.blocks_count {
            0
        } else {
            self.group_of_block(goal)
        };
        for i in 0..self.groups {
            let g = (g0 + i) % self.groups;
            if self.gd_free_blocks(g) == 0 {
                continue;
            }
            let mut bm = self.read_block_bitmap(g)?;
            let first = self.group_first_block(g);
            let n = self.blocks_in_group(g) as usize;
            let start = if i == 0 && goal >= first { (goal - first) as usize } else { 0 };
            let find_free = |bm: &[u8], from: usize| (from..n).find(|&b| !get_bit(bm, b));
            let Some(bit) = find_free(&bm, start).or_else(|| if start > 0 { find_free(&bm, 0) } else { None }) else {
                continue;
            };
            let mut count = 0usize;
            while count < want as usize && bit + count < n && !get_bit(&bm, bit + count) {
                set_bit(&mut bm, bit + count);
                count += 1;
            }
            self.write_block_bitmap(g, bm)?;
            let flags = self.gd_flags(g) & !BG_BLOCK_UNINIT;
            self.gd_set_flags(g, flags);
            let free = self.gd_free_blocks(g) - count as u32;
            self.gd_set_free_blocks(g, free);
            self.flush_gd(g)?;
            return Ok((first + bit as u64, count as u32));
        }
        Err(FsError::NoSpace)
    }

    /// Allocate exactly `count` blocks, possibly as several runs.
    pub(crate) fn alloc_blocks(&mut self, goal: u64, count: u32) -> FsResult<Vec<(u64, u32)>> {
        let mut runs: Vec<(u64, u32)> = Vec::new();
        let mut left = count;
        let mut goal = goal;
        while left > 0 {
            match self.alloc_run(goal, left) {
                Ok((b, n)) => {
                    runs.push((b, n));
                    left -= n;
                    goal = b + n as u64;
                }
                Err(e) => {
                    for (b, n) in runs {
                        let _ = self.free_run(b, n);
                    }
                    return Err(e);
                }
            }
        }
        Ok(runs)
    }

    /// Return `len` blocks starting at `start` to the free pool.
    pub(crate) fn free_run(&mut self, start: u64, len: u32) -> FsResult<()> {
        self.ensure_writable()?;
        let mut blk = start;
        let mut left = len as u64;
        while left > 0 {
            if blk < self.first_data_block as u64 || blk >= self.blocks_count {
                return Err(FsError::Corrupt);
            }
            let g = self.group_of_block(blk);
            let first = self.group_first_block(g);
            let n = self.blocks_in_group(g) as u64;
            let in_group = (first + n - blk).min(left);
            let mut bm = self.read_block_bitmap(g)?;
            for b in blk..blk + in_group {
                let bit = (b - first) as usize;
                if !get_bit(&bm, bit) {
                    return Err(FsError::Corrupt); // double free
                }
                clear_bit(&mut bm, bit);
            }
            self.write_block_bitmap(g, bm)?;
            let flags = self.gd_flags(g) & !BG_BLOCK_UNINIT;
            self.gd_set_flags(g, flags);
            let free = self.gd_free_blocks(g) + in_group as u32;
            self.gd_set_free_blocks(g, free);
            self.flush_gd(g)?;
            for b in blk..blk + in_group {
                self.cache.remove(&b);
            }
            blk += in_group;
            left -= in_group;
        }
        Ok(())
    }

    /// Allocate an inode number, preferring group `near`.
    pub(crate) fn alloc_inode(&mut self, near: u32, is_dir: bool) -> FsResult<u32> {
        self.ensure_writable()?;
        for i in 0..self.groups {
            let g = (near + i) % self.groups;
            if self.gd_free_inodes(g) == 0 {
                continue;
            }
            let mut bm = self.read_inode_bitmap(g)?;
            let Some(idx) = (0..self.ipg as usize).find(|&b| !get_bit(&bm, b)) else {
                continue;
            };
            let ino = g * self.ipg + idx as u32 + 1;
            if ino < self.first_ino {
                continue; // reserved inodes are marked in use, but be safe
            }
            // With lazy inode-table init, slots past `ipg - itable_unused`
            // may hold garbage. Now that this one is about to enter the
            // scanned range, blank everything from the old boundary up to
            // it. (Never zero the whole table: group 0 usually has the
            // ZEROED flag clear while holding the root inode.)
            let flags = self.gd_flags(g);
            if self.has_gdt_csum() {
                let used_upto = (self.ipg - self.gd_itable_unused(g).min(self.ipg)) as u64;
                if idx as u64 > used_upto && flags & BG_INODE_ZEROED == 0 {
                    self.zero_inode_slots(g, used_upto, idx as u64)?;
                }
            }
            set_bit(&mut bm, idx);
            self.gd_set_flags(g, flags & !BG_INODE_UNINIT);
            self.write_inode_bitmap(g, bm)?;
            let free = self.gd_free_inodes(g) - 1;
            self.gd_set_free_inodes(g, free);
            if self.has_gdt_csum() {
                let used_upto = self.ipg - self.gd_itable_unused(g).min(self.ipg);
                if idx as u32 >= used_upto {
                    self.gd_set_itable_unused(g, self.ipg - idx as u32 - 1);
                }
            }
            if is_dir {
                let d = self.gd_used_dirs(g) + 1;
                self.gd_set_used_dirs(g, d);
            }
            self.flush_gd(g)?;
            return Ok(ino);
        }
        Err(FsError::NoSpace)
    }

    /// Zero the inode records `[from, to)` of group `g`'s table.
    fn zero_inode_slots(&mut self, g: u32, from: u64, to: u64) -> FsResult<()> {
        let isz = self.inode_size as u64;
        let bs = self.bs as u64;
        let it = self.gd_inode_table(g);
        let (b0, b1) = (from * isz / bs, (to * isz).div_ceil(bs));
        for b in b0..b1 {
            let mut data = self.read_block(it + b)?;
            let (lo, hi) = ((b * bs).max(from * isz), ((b + 1) * bs).min(to * isz));
            data[(lo - b * bs) as usize..(hi - b * bs) as usize].fill(0);
            self.write_block(it + b, &data)?;
        }
        Ok(())
    }

    pub(crate) fn free_inode_number(&mut self, ino: u32, was_dir: bool) -> FsResult<()> {
        self.ensure_writable()?;
        let g = (ino - 1) / self.ipg;
        let idx = ((ino - 1) % self.ipg) as usize;
        let mut bm = self.read_inode_bitmap(g)?;
        if !get_bit(&bm, idx) {
            return Err(FsError::Corrupt);
        }
        clear_bit(&mut bm, idx);
        self.write_inode_bitmap(g, bm)?;
        let free = self.gd_free_inodes(g) + 1;
        self.gd_set_free_inodes(g, free);
        if was_dir {
            let d = self.gd_used_dirs(g).saturating_sub(1);
            self.gd_set_used_dirs(g, d);
        }
        self.flush_gd(g)
    }

    pub(crate) fn inode_group(&self, ino: u32) -> u32 {
        (ino - 1) / self.ipg
    }
}
