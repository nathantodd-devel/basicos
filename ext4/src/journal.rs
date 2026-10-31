//! jbd2 journal replay.
//!
//! When a filesystem with a journal isn't shut down cleanly, the
//! journal holds committed transactions whose blocks may not have reached
//! their final locations. Mounting must copy them there ("replay")
//! before anything reads the filesystem, or old data is served and new
//! writes can be lost. This implements the three passes Linux does —
//! scan for the end of the log, collect revocations, replay — for the
//! journal formats in the wild: v1/v2 superblocks, 32/64-bit block
//! numbers, and the v2/v3 checksummed layouts. Fast-commit journals and
//! external journal devices are refused (with an error) rather than
//! half-handled.
//!
//! Journal structures are **big-endian**, unlike the rest of ext4.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use basicos_fs::vfs::{FsError, FsResult};

use crate::crc::crc32c;
use crate::extent::{self, Extent};
use crate::fs::Ext4Fs;
use crate::layout::*;

const JBD2_MAGIC: u32 = 0xC03B_3998;
const BLOCKTYPE_DESCRIPTOR: u32 = 1;
const BLOCKTYPE_COMMIT: u32 = 2;
const BLOCKTYPE_SUPERBLOCK_V1: u32 = 3;
const BLOCKTYPE_SUPERBLOCK_V2: u32 = 4;
const BLOCKTYPE_REVOKE: u32 = 5;

const FEAT_INCOMPAT_REVOKE: u32 = 0x1;
const FEAT_INCOMPAT_64BIT: u32 = 0x2;
const FEAT_INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
const FEAT_INCOMPAT_CSUM_V2: u32 = 0x8;
const FEAT_INCOMPAT_CSUM_V3: u32 = 0x10;
const FEAT_INCOMPAT_FAST_COMMIT: u32 = 0x20;
const KNOWN_INCOMPAT: u32 = FEAT_INCOMPAT_REVOKE
    | FEAT_INCOMPAT_64BIT
    | FEAT_INCOMPAT_ASYNC_COMMIT
    | FEAT_INCOMPAT_CSUM_V2
    | FEAT_INCOMPAT_CSUM_V3;

const TAG_ESCAPE: u32 = 1;
const TAG_SAME_UUID: u32 = 2;
const TAG_LAST: u32 = 8;

fn be16(b: &[u8], o: usize) -> u32 {
    u16::from_be_bytes([b[o], b[o + 1]]) as u32
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

struct Journal {
    extents: Vec<Extent>,
    bs: usize,
    first: u32,
    maxlen: u32,
    start: u32,
    sequence: u32,
    incompat: u32,
    csum_v2v3: bool,
    csum_v3: bool,
    is64: bool,
    seed: u32,
    tag_bytes: usize,
}

impl Journal {
    fn next(&self, n: u32) -> u32 {
        if n + 1 >= self.maxlen { self.first } else { n + 1 }
    }
}

/// One tag of a descriptor block.
struct Tag {
    blocknr: u64,
    flags: u32,
    /// The checksum stored in the tag (low 16 bits for v2).
    csum: u32,
}

impl Ext4Fs {
    fn jread(&mut self, j: &Journal, n: u32) -> FsResult<Vec<u8>> {
        let e = extent::find(&j.extents, n).ok_or(FsError::Corrupt)?;
        self.read_block(e.pblk + (n - e.lblk) as u64)
    }

    /// Verify a descriptor/revoke block's tail checksum (v2/v3 only).
    fn block_tail_csum_ok(&self, j: &Journal, block: &[u8]) -> bool {
        if !j.csum_v2v3 {
            return true;
        }
        let mut copy = block.to_vec();
        let t = j.bs - 4;
        copy[t..].fill(0);
        crc32c(j.seed, &copy) == be32(block, t)
    }

    fn commit_csum_ok(&self, j: &Journal, block: &[u8]) -> bool {
        if !j.csum_v2v3 {
            return true;
        }
        // The checksum lives in c_chksum[0], at offset 16.
        let mut copy = block.to_vec();
        copy[16..20].fill(0);
        crc32c(j.seed, &copy) == be32(block, 16)
    }

    fn parse_tags(&self, j: &Journal, block: &[u8]) -> FsResult<Vec<Tag>> {
        let mut tags = Vec::new();
        let limit = j.bs - if j.csum_v2v3 { 4 } else { 0 };
        let mut off = 12;
        while off + j.tag_bytes <= limit {
            let (mut blocknr, flags, csum);
            if j.csum_v3 {
                blocknr = be32(block, off) as u64;
                flags = be32(block, off + 4);
                if j.is64 {
                    blocknr |= (be32(block, off + 8) as u64) << 32;
                }
                csum = be32(block, off + 12);
            } else {
                blocknr = be32(block, off) as u64;
                csum = be16(block, off + 4);
                flags = be16(block, off + 6);
                if j.is64 {
                    blocknr |= (be32(block, off + 8) as u64) << 32;
                }
            }
            tags.push(Tag { blocknr, flags, csum });
            off += j.tag_bytes;
            if flags & TAG_SAME_UUID == 0 {
                off += 16;
            }
            if flags & TAG_LAST != 0 {
                break;
            }
        }
        Ok(tags)
    }

    fn data_csum_ok(&self, j: &Journal, seq: u32, tag: &Tag, data: &[u8]) -> bool {
        if !j.csum_v2v3 {
            return true;
        }
        let c = crc32c(crc32c(j.seed, &seq.to_be_bytes()), data);
        if j.csum_v3 { c == tag.csum } else { (c & 0xFFFF) == tag.csum }
    }

    /// Replay the journal into the filesystem and mark it clean.
    pub(crate) fn replay_journal(&mut self) -> FsResult<()> {
        let jino = le32(&self.sb, 0xE0);
        if jino == 0 || self.compat & COMPAT_HAS_JOURNAL == 0 {
            return Err(FsError::Unsupported); // external journal device
        }
        let inode = self.load_inode(jino)?;
        let extents = self.extents_of(&inode)?;
        let mut j = Journal {
            extents,
            bs: self.bs,
            first: 0,
            maxlen: 0,
            start: 0,
            sequence: 0,
            incompat: 0,
            csum_v2v3: false,
            csum_v3: false,
            is64: false,
            seed: 0,
            tag_bytes: 8,
        };
        let jsb = self.jread(&j, 0)?;
        if be32(&jsb, 0) != JBD2_MAGIC {
            return Err(FsError::Corrupt);
        }
        let btype = be32(&jsb, 4);
        if btype != BLOCKTYPE_SUPERBLOCK_V1 && btype != BLOCKTYPE_SUPERBLOCK_V2 {
            return Err(FsError::Corrupt);
        }
        if be32(&jsb, 0xC) as usize != self.bs {
            return Err(FsError::Corrupt);
        }
        j.maxlen = be32(&jsb, 0x10);
        j.first = be32(&jsb, 0x14);
        j.sequence = be32(&jsb, 0x18);
        j.start = be32(&jsb, 0x1C);
        if btype == BLOCKTYPE_SUPERBLOCK_V2 {
            j.incompat = be32(&jsb, 0x28);
        }
        if j.incompat & FEAT_INCOMPAT_FAST_COMMIT != 0 || j.incompat & !KNOWN_INCOMPAT != 0 {
            return Err(FsError::Unsupported);
        }
        j.is64 = j.incompat & FEAT_INCOMPAT_64BIT != 0;
        j.csum_v3 = j.incompat & FEAT_INCOMPAT_CSUM_V3 != 0;
        j.csum_v2v3 = j.csum_v3 || j.incompat & FEAT_INCOMPAT_CSUM_V2 != 0;
        j.tag_bytes = if j.csum_v3 {
            16
        } else {
            let base = 12 + if j.incompat & FEAT_INCOMPAT_CSUM_V2 != 0 { 2 } else { 0 };
            if j.is64 { base } else { base - 4 }
        };
        if j.csum_v2v3 {
            j.seed = crc32c(!0, &jsb[0x30..0x40]);
            let mut copy = jsb.clone();
            copy[0xFC..0x100].fill(0);
            if crc32c(!0, &copy[..1024]) != be32(&jsb, 0xFC) {
                return Err(FsError::Corrupt);
            }
        }
        if j.first == 0 || j.first >= j.maxlen || j.maxlen as u64 > (extent_len(&j.extents)) {
            return Err(FsError::Corrupt);
        }

        let mut end_tid = j.sequence;
        if j.start != 0 {
            end_tid = self.scan_pass(&j)?;
            let revoked = self.revoke_pass(&j, end_tid)?;
            self.replay_pass(&j, end_tid, &revoked)?;
            self.replayed = end_tid.wrapping_sub(j.sequence);
        }

        // Empty the journal: nothing left to replay, and the next
        // transaction id continues after the last one we saw.
        let mut jsb = jsb;
        put_be32(&mut jsb, 0x18, end_tid);
        put_be32(&mut jsb, 0x1C, 0);
        if j.csum_v2v3 {
            jsb[0xFC..0x100].fill(0);
            let c = crc32c(!0, &jsb[..1024]);
            put_be32(&mut jsb, 0xFC, c);
        }
        let e = extent::find(&j.extents, 0).ok_or(FsError::Corrupt)?;
        self.write_block(e.pblk, &jsb)?;

        // Replay may have overwritten the filesystem superblock with the
        // journalled (needs_recovery-set) copy; re-read and clear it.
        let mut sb = self.read_sb_fresh()?;
        let incompat = le32(&sb, 0x60) & !INCOMPAT_RECOVER;
        put32(&mut sb, 0x60, incompat);
        self.sb = sb;
        self.cache.clear();
        self.ext_cache.clear();
        // flush_sb wants group counters that describe the (replayed) disk,
        // so write the raw superblock as-is instead.
        self.write_sb_raw()
    }

    /// PASS_SCAN: find where the log ends. Returns the first transaction
    /// id that is *not* fully committed.
    fn scan_pass(&mut self, j: &Journal) -> FsResult<u32> {
        let mut blk = j.start;
        let mut tid = j.sequence;
        let mut steps = 0u64;
        loop {
            steps += 1;
            if steps > j.maxlen as u64 * 2 {
                break;
            }
            let b = self.jread(j, blk)?;
            if be32(&b, 0) != JBD2_MAGIC || be32(&b, 8) != tid {
                break;
            }
            match be32(&b, 4) {
                BLOCKTYPE_DESCRIPTOR => {
                    if !self.block_tail_csum_ok(j, &b) {
                        break;
                    }
                    let n = self.parse_tags(j, &b)?.len() as u32;
                    blk = j.next(blk);
                    for _ in 0..n {
                        blk = j.next(blk);
                    }
                }
                BLOCKTYPE_COMMIT => {
                    if !self.commit_csum_ok(j, &b) {
                        break;
                    }
                    tid = tid.wrapping_add(1);
                    blk = j.next(blk);
                }
                BLOCKTYPE_REVOKE => {
                    if !self.block_tail_csum_ok(j, &b) {
                        break;
                    }
                    blk = j.next(blk);
                }
                _ => break,
            }
        }
        Ok(tid)
    }

    /// PASS_REVOKE: which blocks were revoked, and in which transaction.
    fn revoke_pass(&mut self, j: &Journal, end_tid: u32) -> FsResult<BTreeMap<u64, u32>> {
        let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
        let mut blk = j.start;
        let mut tid = j.sequence;
        while tid != end_tid {
            let b = self.jread(j, blk)?;
            if be32(&b, 0) != JBD2_MAGIC || be32(&b, 8) != tid {
                break;
            }
            match be32(&b, 4) {
                BLOCKTYPE_DESCRIPTOR => {
                    let n = self.parse_tags(j, &b)?.len() as u32;
                    blk = j.next(blk);
                    for _ in 0..n {
                        blk = j.next(blk);
                    }
                }
                BLOCKTYPE_COMMIT => {
                    tid = tid.wrapping_add(1);
                    blk = j.next(blk);
                }
                BLOCKTYPE_REVOKE => {
                    let used = (be32(&b, 12) as usize).min(j.bs);
                    let rec = if j.is64 { 8 } else { 4 };
                    let mut off = 16;
                    while off + rec <= used {
                        let nr = if j.is64 {
                            ((be32(&b, off) as u64) << 32) | be32(&b, off + 4) as u64
                        } else {
                            be32(&b, off) as u64
                        };
                        let e = revoked.entry(nr).or_insert(tid);
                        if tid_after(tid, *e) {
                            *e = tid;
                        }
                        off += rec;
                    }
                    blk = j.next(blk);
                }
                _ => break,
            }
        }
        Ok(revoked)
    }

    /// PASS_REPLAY: copy every journalled block to its home location,
    /// unless a later revoke says it was freed.
    fn replay_pass(&mut self, j: &Journal, end_tid: u32, revoked: &BTreeMap<u64, u32>) -> FsResult<()> {
        let mut blk = j.start;
        let mut tid = j.sequence;
        while tid != end_tid {
            let b = self.jread(j, blk)?;
            if be32(&b, 0) != JBD2_MAGIC || be32(&b, 8) != tid {
                return Err(FsError::Corrupt);
            }
            match be32(&b, 4) {
                BLOCKTYPE_DESCRIPTOR => {
                    let tags = self.parse_tags(j, &b)?;
                    blk = j.next(blk);
                    for tag in tags {
                        let mut data = self.jread(j, blk)?;
                        blk = j.next(blk);
                        if !self.data_csum_ok(j, tid, &tag, &data) {
                            return Err(FsError::Corrupt);
                        }
                        if let Some(&rt) = revoked.get(&tag.blocknr) {
                            if !tid_after(tid, rt) {
                                continue; // revoked at or after this transaction
                            }
                        }
                        if tag.flags & TAG_ESCAPE != 0 {
                            put_be32(&mut data, 0, JBD2_MAGIC);
                        }
                        if tag.blocknr >= self.blocks_count {
                            return Err(FsError::Corrupt);
                        }
                        self.write_block(tag.blocknr, &data)?;
                    }
                }
                BLOCKTYPE_COMMIT => {
                    tid = tid.wrapping_add(1);
                    blk = j.next(blk);
                }
                BLOCKTYPE_REVOKE => blk = j.next(blk),
                _ => return Err(FsError::Corrupt),
            }
        }
        Ok(())
    }

    fn read_sb_fresh(&mut self) -> FsResult<Vec<u8>> {
        let mut raw = alloc::vec![0u8; SB_SIZE];
        self.dev
            .read_blocks((SB_OFFSET / basicos_fs::block::BLOCK_SIZE) as u64, &mut raw)
            .map_err(|_| FsError::Io)?;
        Ok(raw)
    }

    /// Write `self.sb` back verbatim apart from a fresh checksum.
    fn write_sb_raw(&mut self) -> FsResult<()> {
        if le32(&self.sb, 0x64) & RO_COMPAT_METADATA_CSUM != 0 {
            let c = crc32c(!0, &self.sb[..0x3FC]);
            put32(&mut self.sb, 0x3FC, c);
        }
        self.dev
            .write_blocks((SB_OFFSET / basicos_fs::block::BLOCK_SIZE) as u64, &self.sb)
            .map_err(|e| match e {
                basicos_fs::block::BlockError::ReadOnly => FsError::ReadOnly,
                _ => FsError::Io,
            })
    }
}

/// Number of journal blocks the inode maps (its highest mapped block + 1).
fn extent_len(extents: &[Extent]) -> u64 {
    extents.last().map(|e| e.end()).unwrap_or(0)
}

/// Is transaction id `a` later than `b` (with wraparound)?
fn tid_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
