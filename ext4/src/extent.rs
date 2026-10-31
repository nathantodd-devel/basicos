//! Mapping file blocks to disk blocks: extent trees (read and rebuild)
//! and the legacy indirect-block maps of ext2/ext3 files (read only).

use alloc::vec::Vec;
use basicos_fs::vfs::{FsError, FsResult};

use crate::crc::crc32c;
use crate::fs::Ext4Fs;
use crate::inode::Inode;
use crate::layout::*;

/// A run of file blocks `[lblk, lblk+len)` stored contiguously at `pblk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub lblk: u32,
    pub len: u32,
    pub pblk: u64,
    /// Allocated but never written (reads as zeros).
    pub uninit: bool,
}

impl Extent {
    pub fn end(&self) -> u64 {
        self.lblk as u64 + self.len as u64
    }
}

/// The extent of a sorted list that covers `lblk`, if any.
pub fn find(extents: &[Extent], lblk: u32) -> Option<&Extent> {
    let i = extents.partition_point(|e| e.lblk <= lblk);
    let e = extents.get(i.checked_sub(1)?)?;
    ((lblk as u64) < e.end()).then_some(e)
}

/// Merge neighbours that are contiguous on disk and in the file and in
/// the same initialised state (never past the format's length limit).
pub fn coalesce(list: &mut Vec<Extent>) {
    let mut out: Vec<Extent> = Vec::with_capacity(list.len());
    for e in list.drain(..) {
        if let Some(last) = out.last_mut() {
            if last.end() == e.lblk as u64
                && last.pblk + last.len as u64 == e.pblk
                && last.uninit == e.uninit
                && last.len + e.len <= max_len(e.uninit)
            {
                last.len += e.len;
                continue;
            }
        }
        out.push(e);
    }
    *list = out;
}

/// Longest single extent: 32768 blocks initialised, 32767 uninitialised
/// (bit 15 of the length field is the flag).
pub fn max_len(uninit: bool) -> u32 {
    if uninit { EXT_INIT_MAX_LEN - 1 } else { EXT_INIT_MAX_LEN }
}

/// Split anything longer than the format allows into legal pieces.
pub fn split_long(list: &[Extent]) -> Vec<Extent> {
    let mut out = Vec::with_capacity(list.len());
    for e in list {
        let mut e = *e;
        while e.len > max_len(e.uninit) {
            let n = max_len(e.uninit);
            out.push(Extent { len: n, ..e });
            e.lblk += n;
            e.pblk += n as u64;
            e.len -= n;
        }
        out.push(e);
    }
    out
}

const HDR: usize = 12;

impl Ext4Fs {
    fn parse_extent_node(
        &mut self,
        node: &[u8],
        inode: &Inode,
        depth_expected: Option<u16>,
        out: &mut Vec<Extent>,
        nodes: &mut Vec<u64>,
        is_root: bool,
    ) -> FsResult<()> {
        if node.len() < HDR || le16(node, 0) != EXT_MAGIC {
            return Err(FsError::Corrupt);
        }
        let entries = le16(node, 2) as usize;
        let max = le16(node, 4) as usize;
        let depth = le16(node, 6);
        if entries > max || HDR + entries * 12 > node.len() || depth > 5 {
            return Err(FsError::Corrupt);
        }
        if let Some(d) = depth_expected {
            if d != depth {
                return Err(FsError::Corrupt);
            }
        }
        if !is_root && self.has_metadata_csum() {
            let tail = HDR + max * 12;
            if tail + 4 > node.len() {
                return Err(FsError::Corrupt);
            }
            let want = crc32c(self.inode_seed(inode.ino, inode.generation()), &node[..tail]);
            if le32(node, tail) != want {
                return Err(FsError::Corrupt);
            }
        }
        for i in 0..entries {
            let e = &node[HDR + i * 12..HDR + (i + 1) * 12];
            if depth == 0 {
                let raw_len = le16(e, 4) as u32;
                let (len, uninit) = if raw_len > EXT_INIT_MAX_LEN {
                    (raw_len - EXT_INIT_MAX_LEN, true)
                } else {
                    (raw_len, false)
                };
                let pblk = le32(e, 8) as u64 | (le16(e, 6) as u64) << 32;
                out.push(Extent { lblk: le32(e, 0), len, pblk, uninit });
            } else {
                let child = le32(e, 4) as u64 | (le16(e, 8) as u64) << 32;
                nodes.push(child);
                let block = self.read_block(child)?;
                self.parse_extent_node(&block, inode, Some(depth - 1), out, nodes, false)?;
            }
        }
        Ok(())
    }

    /// All extents of `inode` in file order (uninitialised ones flagged).
    pub(crate) fn extents_of(&mut self, inode: &Inode) -> FsResult<Vec<Extent>> {
        if let Some(c) = self.ext_cache.get(&inode.ino) {
            return Ok(c.clone());
        }
        let list = if inode.has_flag(INODE_EXTENTS) {
            let mut out = Vec::new();
            let mut nodes = Vec::new();
            self.parse_extent_node(&inode.i_block().to_vec(), inode, None, &mut out, &mut nodes, true)?;
            out
        } else {
            self.block_map_extents(inode)?
        };
        // Sanity: sorted, non-overlapping.
        for w in list.windows(2) {
            if w[0].end() > w[1].lblk as u64 {
                return Err(FsError::Corrupt);
            }
        }
        self.ext_cache.insert(inode.ino, list.clone());
        Ok(list)
    }

    /// Blocks holding the tree's interior/leaf nodes (not the root, which
    /// lives in the inode).
    pub(crate) fn extent_node_blocks(&mut self, inode: &Inode) -> FsResult<Vec<u64>> {
        if !inode.has_flag(INODE_EXTENTS) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut nodes = Vec::new();
        self.parse_extent_node(&inode.i_block().to_vec(), inode, None, &mut out, &mut nodes, true)?;
        Ok(nodes)
    }

    /// Legacy ext2/ext3 mapping: 12 direct pointers, then single, double
    /// and triple indirect blocks. Returned as coalesced extents.
    fn block_map_extents(&mut self, inode: &Inode) -> FsResult<Vec<Extent>> {
        let ptrs_per_block = (self.bs / 4) as u64;
        let mut list: Vec<Extent> = Vec::new();
        let mut lblk: u64 = 0;
        let mut push = |list: &mut Vec<Extent>, l: u64, p: u64| {
            if p == 0 || l > u32::MAX as u64 {
                return;
            }
            list.push(Extent { lblk: l as u32, len: 1, pblk: p, uninit: false });
        };
        let ib = inode.i_block().to_vec();
        for i in 0..12 {
            push(&mut list, lblk, le32(&ib, i * 4) as u64);
            lblk += 1;
        }
        // level: 1 = single indirect, 2 = double, 3 = triple.
        for level in 1..=3u32 {
            let root = le32(&ib, (11 + level as usize) * 4) as u64;
            let span = ptrs_per_block.pow(level);
            if root != 0 {
                self.walk_indirect(root, level, lblk, &mut list, &mut push)?;
            }
            lblk += span;
        }
        coalesce(&mut list);
        Ok(list)
    }

    fn walk_indirect(
        &mut self,
        blk: u64,
        level: u32,
        base: u64,
        list: &mut Vec<Extent>,
        push: &mut impl FnMut(&mut Vec<Extent>, u64, u64),
    ) -> FsResult<()> {
        let data = self.read_block(blk)?;
        let per = (self.bs / 4) as u64;
        let child_span = per.pow(level - 1);
        for i in 0..per {
            let p = le32(&data, i as usize * 4) as u64;
            if p == 0 {
                continue;
            }
            if level == 1 {
                push(list, base + i, p);
            } else {
                self.walk_indirect(p, level - 1, base + i * child_span, list, push)?;
            }
        }
        Ok(())
    }

    /// Rebuild `inode`'s extent tree to describe exactly `extents`
    /// (sorted, non-overlapping): root in the inode, interior/leaf nodes
    /// in freshly-(re)used blocks, `i_blocks` recomputed. The caller
    /// writes the inode afterwards. Nothing is modified if allocation
    /// fails.
    pub(crate) fn set_extents(&mut self, inode: &mut Inode, extents: &[Extent]) -> FsResult<()> {
        self.ensure_writable()?;
        let mut list = split_long(extents);
        coalesce(&mut list);

        let old_nodes = self.extent_node_blocks(inode)?;
        let cap = (self.bs - HDR) / 12;

        // Work out the shape: how many nodes per level, bottom-up.
        let mut levels: Vec<usize> = Vec::new();
        if list.len() > 4 {
            let mut n = list.len();
            loop {
                let nodes = n.div_ceil(cap);
                levels.push(nodes);
                if nodes <= 4 {
                    break;
                }
                n = nodes;
            }
        }
        let total_nodes: usize = levels.iter().sum();

        // Node blocks: reuse the old ones, allocate the shortfall.
        let goal = list.first().map(|e| e.pblk).unwrap_or_else(|| self.inode_goal(inode.ino));
        let mut blocks: Vec<u64> = old_nodes.iter().copied().take(total_nodes).collect();
        let mut fresh: Vec<u64> = Vec::new();
        while blocks.len() < total_nodes {
            match self.alloc_run(goal, 1) {
                Ok((b, _)) => {
                    blocks.push(b);
                    fresh.push(b);
                }
                Err(e) => {
                    for b in fresh {
                        let _ = self.free_run(b, 1);
                    }
                    return Err(e);
                }
            }
        }
        for &b in old_nodes.iter().skip(total_nodes) {
            self.free_run(b, 1)?;
        }

        // Fill the nodes, level by level.
        let seed = self.inode_seed(inode.ino, inode.generation());
        let csum = self.has_metadata_csum();
        let entry_bytes = |e: &Extent| -> [u8; 12] {
            let mut b = [0u8; 12];
            put32(&mut b, 0, e.lblk);
            let raw_len = if e.uninit { e.len + EXT_INIT_MAX_LEN } else { e.len };
            put16(&mut b, 4, raw_len as u16);
            put16(&mut b, 6, (e.pblk >> 32) as u16);
            put32(&mut b, 8, e.pblk as u32);
            b
        };
        let node_header = |entries: usize, max: usize, depth: usize| -> [u8; 12] {
            let mut h = [0u8; 12];
            put16(&mut h, 0, EXT_MAGIC);
            put16(&mut h, 2, entries as u16);
            put16(&mut h, 4, max as u16);
            put16(&mut h, 6, depth as u16);
            h
        };

        let root_entries: Vec<u8>;
        let root_depth: usize;
        let root_count: usize;
        if levels.is_empty() {
            root_depth = 0;
            root_count = list.len();
            root_entries = list.iter().flat_map(|e| entry_bytes(e)).collect();
        } else {
            // (first lblk, block) for each node of the level below.
            let mut below: Vec<(u32, u64)> = Vec::new();
            let mut next_block = 0usize;
            for (lvl, &count) in levels.iter().enumerate() {
                let mut this: Vec<(u32, u64)> = Vec::new();
                for n in 0..count {
                    let blk = blocks[next_block];
                    next_block += 1;
                    let mut node = alloc::vec![0u8; self.bs];
                    let (entries, first_lblk): (Vec<u8>, u32);
                    let cnt;
                    if lvl == 0 {
                        let chunk = &list[n * cap..((n + 1) * cap).min(list.len())];
                        entries = chunk.iter().flat_map(|e| entry_bytes(e)).collect();
                        first_lblk = chunk[0].lblk;
                        cnt = chunk.len();
                    } else {
                        let chunk = &below[n * cap..((n + 1) * cap).min(below.len())];
                        let mut v = Vec::new();
                        for &(l, b) in chunk {
                            let mut e = [0u8; 12];
                            put32(&mut e, 0, l);
                            put32(&mut e, 4, b as u32);
                            put16(&mut e, 8, (b >> 32) as u16);
                            v.extend_from_slice(&e);
                        }
                        entries = v;
                        first_lblk = chunk[0].0;
                        cnt = chunk.len();
                    }
                    node[..HDR].copy_from_slice(&node_header(cnt, cap, lvl));
                    node[HDR..HDR + entries.len()].copy_from_slice(&entries);
                    if csum {
                        let tail = HDR + cap * 12;
                        let c = crc32c(seed, &node[..tail]);
                        put32(&mut node, tail, c);
                    }
                    self.write_block(blk, &node)?;
                    this.push((first_lblk, blk));
                }
                below = this;
            }
            root_depth = levels.len();
            root_count = below.len();
            let mut v = Vec::new();
            for &(l, b) in &below {
                let mut e = [0u8; 12];
                put32(&mut e, 0, l);
                put32(&mut e, 4, b as u32);
                put16(&mut e, 8, (b >> 32) as u16);
                v.extend_from_slice(&e);
            }
            root_entries = v;
        }

        let ib = inode.i_block_mut();
        ib.fill(0);
        ib[..HDR].copy_from_slice(&node_header(root_count, 4, root_depth));
        ib[HDR..HDR + root_entries.len()].copy_from_slice(&root_entries);
        inode.set_flags(inode.flags() | INODE_EXTENTS);

        let data_blocks: u64 = list.iter().map(|e| e.len as u64).sum();
        let acl = if inode.file_acl() != 0 { 1 } else { 0 };
        self.set_inode_block_count(inode, data_blocks + total_nodes as u64 + acl);
        self.ext_cache.insert(inode.ino, list);
        Ok(())
    }

    /// Store `blocks` (filesystem blocks in use) into `i_blocks` in the
    /// unit this inode uses.
    pub(crate) fn set_inode_block_count(&self, inode: &mut Inode, blocks: u64) {
        let huge = self.ro_compat & RO_COMPAT_HUGE_FILE != 0 && inode.has_flag(INODE_HUGE_FILE);
        let n = if huge { blocks } else { blocks * (self.bs as u64 / 512) };
        inode.set_blocks_raw(n);
    }

    /// `i_blocks` converted to filesystem blocks.
    pub(crate) fn inode_block_count(&self, inode: &Inode) -> u64 {
        let huge = self.ro_compat & RO_COMPAT_HUGE_FILE != 0 && inode.has_flag(INODE_HUGE_FILE);
        if huge { inode.blocks_raw() } else { inode.blocks_raw() / (self.bs as u64 / 512) }
    }
}
