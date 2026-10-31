//! Directories: reading entries, and inserting/removing them in place.
//!
//! Directory blocks are a packed list of `{inode, rec_len, name_len,
//! type, name}` records; hash-tree (`dx`) directories are the same
//! records plus index blocks disguised as empty entries, so a plain
//! linear scan reads them correctly. Writing is where they differ: an
//! insertion has to land in the leaf its hash selects, which this driver
//! doesn't maintain — so the first insertion into an indexed directory
//! converts it to a linear one (dropping the index, which is always
//! legal), and deletions never need to touch the index.

use alloc::vec::Vec;
use basicos_fs::vfs::{FsError, FsResult};

use crate::crc::crc32c;
use crate::extent::{self, Extent};
use crate::fs::Ext4Fs;
use crate::inode::Inode;
use crate::layout::*;

#[derive(Debug, Clone)]
pub struct RawDirent {
    pub ino: u32,
    pub name: Vec<u8>,
    pub ftype: u8,
}

const TAIL_LEN: usize = 12;
const TAIL_TYPE: u8 = 0xDE;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// `rec_len` as stored → real length (0 or 65536 for a 64 KiB block).
fn rec_len_from_disk(raw: u16, bs: usize) -> usize {
    if raw == 0 || (raw == 0xFFFF && bs == 65536) {
        bs
    } else {
        raw as usize
    }
}

fn rec_len_to_disk(len: usize, bs: usize) -> u16 {
    if len == bs && bs == 65536 { 0 } else { len as u16 }
}

pub fn valid_name(name: &[u8]) -> FsResult<()> {
    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
        return Err(FsError::Invalid);
    }
    if name.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

impl Ext4Fs {
    fn dir_tail_len(&self) -> usize {
        if self.has_metadata_csum() { TAIL_LEN } else { 0 }
    }

    /// The physical block of directory block `lblk`.
    fn dir_pblk(&mut self, dir: &Inode, lblk: u32) -> FsResult<u64> {
        let ext = self.extents_of(dir)?;
        extent::find(&ext, lblk).map(|e| e.pblk + (lblk - e.lblk) as u64).ok_or(FsError::Corrupt)
    }

    pub(crate) fn dir_blocks(&self, dir: &Inode) -> u32 {
        dir.size().div_ceil(self.bs as u64) as u32
    }

    fn has_tail(&self, block: &[u8]) -> bool {
        let t = self.bs - TAIL_LEN;
        le32(block, t) == 0 && le16(block, t + 4) == TAIL_LEN as u16 && block[t + 6] == 0 && block[t + 7] == TAIL_TYPE
    }

    /// Read a directory block, verifying its checksum when it has one.
    fn dir_read_block(&mut self, dir: &Inode, lblk: u32) -> FsResult<(u64, Vec<u8>)> {
        let pblk = self.dir_pblk(dir, lblk)?;
        let block = self.read_block(pblk)?;
        if self.has_metadata_csum() && self.has_tail(&block) {
            let want = crc32c(self.inode_seed(dir.ino, dir.generation()), &block[..self.bs - TAIL_LEN]);
            if le32(&block, self.bs - 4) != want {
                return Err(FsError::Corrupt);
            }
        }
        Ok((pblk, block))
    }

    fn dir_write_block(&mut self, dir: &Inode, pblk: u64, block: &mut [u8]) -> FsResult<()> {
        if self.has_metadata_csum() {
            let t = self.bs - TAIL_LEN;
            block[t..t + 4].fill(0);
            put16(block, t + 4, TAIL_LEN as u16);
            block[t + 6] = 0;
            block[t + 7] = TAIL_TYPE;
            let c = crc32c(self.inode_seed(dir.ino, dir.generation()), &block[..t]);
            put32(block, t + 8, c);
        }
        self.write_block(pblk, block)
    }

    /// Walk one block's records: `(offset, ino, rec_len, name_len, type)`.
    fn walk_block(&self, block: &[u8]) -> FsResult<Vec<(usize, u32, usize, usize, u8)>> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= self.bs {
            let ino = le32(block, off);
            let rec_len = rec_len_from_disk(le16(block, off + 4), self.bs);
            let name_len = block[off + 6] as usize;
            let ftype = block[off + 7];
            if rec_len < 8 || rec_len % 4 != 0 || off + rec_len > self.bs || 8 + name_len > rec_len {
                return Err(FsError::Corrupt);
            }
            out.push((off, ino, rec_len, name_len, ftype));
            off += rec_len;
        }
        if off != self.bs {
            return Err(FsError::Corrupt);
        }
        Ok(out)
    }

    /// Every live entry of a directory, `.` and `..` included.
    pub(crate) fn dir_list(&mut self, dir: &Inode) -> FsResult<Vec<RawDirent>> {
        if !dir.is_dir() {
            return Err(FsError::NotADirectory);
        }
        let mut out = Vec::new();
        for lblk in 0..self.dir_blocks(dir) {
            let (_, block) = self.dir_read_block(dir, lblk)?;
            for (off, ino, _, nl, ft) in self.walk_block(&block)? {
                if ino == 0 {
                    continue; // deleted / checksum tail / index filler
                }
                out.push(RawDirent { ino, name: block[off + 8..off + 8 + nl].to_vec(), ftype: ft });
            }
        }
        Ok(out)
    }

    pub(crate) fn dir_lookup(&mut self, dir: &Inode, name: &[u8]) -> FsResult<Option<RawDirent>> {
        if !dir.is_dir() {
            return Err(FsError::NotADirectory);
        }
        for lblk in 0..self.dir_blocks(dir) {
            let (_, block) = self.dir_read_block(dir, lblk)?;
            for (off, ino, _, nl, ft) in self.walk_block(&block)? {
                if ino != 0 && &block[off + 8..off + 8 + nl] == name {
                    return Ok(Some(RawDirent { ino, name: name.to_vec(), ftype: ft }));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn dir_is_empty(&mut self, dir: &Inode) -> FsResult<bool> {
        Ok(self.dir_list(dir)?.iter().all(|e| e.name == b"." || e.name == b".."))
    }

    fn put_entry(&self, block: &mut [u8], off: usize, ino: u32, rec_len: usize, name: &[u8], ftype: u8) {
        put32(block, off, ino);
        put16(block, off + 4, rec_len_to_disk(rec_len, self.bs));
        block[off + 6] = name.len() as u8;
        block[off + 7] = if self.incompat & INCOMPAT_FILETYPE != 0 { ftype } else { 0 };
        block[off + 8..off + 8 + name.len()].copy_from_slice(name);
    }

    /// A fresh directory block holding just `.` and `..`.
    pub(crate) fn dir_new_block(&self, self_ino: u32, parent_ino: u32) -> Vec<u8> {
        let usable = self.bs - self.dir_tail_len();
        let mut b = alloc::vec![0u8; self.bs];
        self.put_entry(&mut b, 0, self_ino, 12, b".", FT_DIR);
        self.put_entry(&mut b, 12, parent_ino, usable - 12, b"..", FT_DIR);
        b
    }

    /// Write the first block of a brand-new directory (`mkdir`).
    pub(crate) fn dir_write_new(&mut self, dir: &Inode, pblk: u64, parent_ino: u32) -> FsResult<()> {
        let mut b = self.dir_new_block(dir.ino, parent_ino);
        self.dir_write_block(dir, pblk, &mut b)
    }

    /// Insert `name -> ino` into `dir`. Writes the directory inode.
    pub(crate) fn dir_add(&mut self, dir: &mut Inode, name: &[u8], ino: u32, ftype: u8) -> FsResult<()> {
        valid_name(name)?;
        if dir.has_flag(INODE_INDEX) {
            self.dir_unindex(dir)?;
        }
        let need = align4(8 + name.len());
        let usable = self.bs - self.dir_tail_len();
        let nblocks = self.dir_blocks(dir);
        for lblk in 0..nblocks {
            let (pblk, mut block) = self.dir_read_block(dir, lblk)?;
            for (off, e_ino, rec_len, nl, _) in self.walk_block(&block)? {
                // Skip the checksum tail (and anything reaching into it).
                if off >= usable || off + rec_len > usable {
                    continue;
                }
                let actual = if e_ino == 0 { 0 } else { align4(8 + nl) };
                if rec_len - actual >= need {
                    if e_ino == 0 {
                        self.put_entry(&mut block, off, ino, rec_len, name, ftype);
                    } else {
                        put16(&mut block, off + 4, rec_len_to_disk(actual, self.bs));
                        self.put_entry(&mut block, off + actual, ino, rec_len - actual, name, ftype);
                    }
                    self.dir_write_block(dir, pblk, &mut block)?;
                    return self.touch_dir(dir);
                }
            }
        }

        // No room: append a block.
        let mut ext = self.extents_of(dir)?;
        if !dir.has_flag(INODE_EXTENTS) {
            return Err(FsError::Unsupported); // legacy block-mapped directory
        }
        let goal = ext.last().map(|e| e.pblk + e.len as u64).unwrap_or_else(|| self.inode_goal(dir.ino));
        let (pblk, _) = self.alloc_run(goal, 1)?;
        let lblk = nblocks;
        ext.push(Extent { lblk, len: 1, pblk, uninit: false });
        if let Err(e) = self.set_extents(dir, &ext) {
            let _ = self.free_run(pblk, 1);
            return Err(e);
        }
        let mut block = alloc::vec![0u8; self.bs];
        self.put_entry(&mut block, 0, ino, usable, name, ftype);
        self.dir_write_block(dir, pblk, &mut block)?;
        let size = dir.size() + self.bs as u64;
        dir.set_size(size);
        self.touch_dir(dir)
    }

    fn touch_dir(&mut self, dir: &mut Inode) -> FsResult<()> {
        let now = self.now();
        dir.set_mtime(now);
        dir.set_ctime(now);
        self.store_inode(dir)
    }

    /// Remove `name` from `dir`, returning what it pointed at. Writes the
    /// directory inode.
    pub(crate) fn dir_remove(&mut self, dir: &mut Inode, name: &[u8]) -> FsResult<Option<RawDirent>> {
        for lblk in 0..self.dir_blocks(dir) {
            let (pblk, mut block) = self.dir_read_block(dir, lblk)?;
            let recs = self.walk_block(&block)?;
            for (idx, &(off, ino, rec_len, nl, ft)) in recs.iter().enumerate() {
                if ino == 0 || &block[off + 8..off + 8 + nl] != name {
                    continue;
                }
                let found = RawDirent { ino, name: name.to_vec(), ftype: ft };
                if idx == 0 {
                    put32(&mut block, off, 0);
                } else {
                    let (poff, _, prec, _, _) = recs[idx - 1];
                    put16(&mut block, poff + 4, rec_len_to_disk(prec + rec_len, self.bs));
                }
                self.dir_write_block(dir, pblk, &mut block)?;
                self.touch_dir(dir)?;
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Point `dir`'s `..` at `parent`.
    pub(crate) fn dir_set_parent(&mut self, dir: &mut Inode, parent: u32) -> FsResult<()> {
        let (pblk, mut block) = self.dir_read_block(dir, 0)?;
        let recs = self.walk_block(&block)?;
        let &(off, _, _, nl, _) = recs.get(1).ok_or(FsError::Corrupt)?;
        if &block[off + 8..off + 8 + nl] != b".." {
            return Err(FsError::Corrupt);
        }
        put32(&mut block, off, parent);
        self.dir_write_block(dir, pblk, &mut block)?;
        self.touch_dir(dir)
    }

    /// The parent recorded in `dir`'s `..` entry.
    pub(crate) fn dir_parent(&mut self, dir: &Inode) -> FsResult<u32> {
        let (_, block) = self.dir_read_block(dir, 0)?;
        let recs = self.walk_block(&block)?;
        let &(off, ino, _, nl, _) = recs.get(1).ok_or(FsError::Corrupt)?;
        if &block[off + 8..off + 8 + nl] != b".." {
            return Err(FsError::Corrupt);
        }
        Ok(ino)
    }

    /// Turn a hash-tree directory into a plain linear one: index blocks
    /// become empty leaves, the root block keeps just `.`/`..`, and the
    /// `INDEX` flag is dropped.
    fn dir_unindex(&mut self, dir: &mut Inode) -> FsResult<()> {
        let usable = self.bs - self.dir_tail_len();
        for lblk in 0..self.dir_blocks(dir) {
            let (pblk, block) = self.dir_read_block(dir, lblk)?;
            let recs = self.walk_block(&block)?;
            let mut fresh: Option<Vec<u8>> = None;
            if lblk == 0 {
                let parent = le32(&block, recs.get(1).ok_or(FsError::Corrupt)?.0);
                fresh = Some(self.dir_new_block(dir.ino, parent));
            } else if recs.len() == 1 && recs[0].1 == 0 && recs[0].2 == self.bs {
                // index node: one fake entry spanning the whole block
                let mut b = alloc::vec![0u8; self.bs];
                self.put_entry(&mut b, 0, 0, usable, b"", FT_UNKNOWN);
                fresh = Some(b);
            }
            if let Some(mut b) = fresh {
                self.dir_write_block(dir, pblk, &mut b)?;
            }
        }
        dir.set_flags(dir.flags() & !INODE_INDEX);
        self.store_inode(dir)
    }
}
