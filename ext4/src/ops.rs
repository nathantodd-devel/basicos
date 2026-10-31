//! File and directory operations, and the [`FileSystem`] implementation.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use basicos_fs::vfs::{Attr, DirEntry, FileSystem, FsError, FsResult};

use crate::crc::crc32c;
use crate::dir::valid_name;
use crate::extent::{self, Extent};
use crate::fs::Ext4Fs;
use crate::inode::Inode;
use crate::layout::*;

const MAX_SYMLINK_HOPS: u32 = 40;
/// Link counts above this are stored as 1 ("too many to count") for
/// directories when `dir_nlink` is on, and refused for files.
const LINK_MAX: u16 = 65000;

impl Ext4Fs {
    // --- helpers -----------------------------------------------------------------

    /// Run a mutating operation, then push the updated superblock
    /// counters to disk.
    fn mutate<T>(&mut self, f: impl FnOnce(&mut Self) -> FsResult<T>) -> FsResult<T> {
        self.ensure_writable()?;
        let r = f(self);
        let s = self.flush_sb();
        match (r, s) {
            (Ok(v), Ok(())) => Ok(v),
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
        }
    }

    fn require_extents(&self, inode: &Inode) -> FsResult<()> {
        if inode.has_flag(INODE_EXTENTS) || inode.blocks_raw() == 0 {
            Ok(())
        } else {
            Err(FsError::Unsupported) // legacy block-mapped file: read-only
        }
    }

    fn ftype_of(inode: &Inode) -> u8 {
        if inode.is_dir() {
            FT_DIR
        } else if inode.is_symlink() {
            FT_SYMLINK
        } else if inode.is_reg() {
            FT_REG
        } else {
            FT_UNKNOWN
        }
    }

    fn entry_of(&self, name: &str, inode: &Inode) -> DirEntry {
        DirEntry {
            name: String::from(name),
            is_dir: inode.is_dir(),
            size: inode.size(),
            first_cluster: inode.ino,
            attr: Attr {
                perm: inode.perm(),
                uid: inode.uid(),
                gid: inode.gid(),
                nlink: inode.links() as u32,
                atime: inode.atime(),
                mtime: inode.mtime(),
                ctime: inode.ctime(),
                is_symlink: inode.is_symlink(),
            },
        }
    }

    // --- reading file data -------------------------------------------------------

    /// Read up to `buf.len()` bytes at `off`; short only at end of file.
    pub(crate) fn read_data(&mut self, inode: &Inode, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let size = inode.size();
        if off >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(size - off) as usize;
        let bs = self.bs as u64;
        let ext = self.extents_of(inode)?;
        let mut done = 0usize;
        while done < n {
            let pos = off + done as u64;
            let lblk = (pos / bs) as u32;
            let boff = (pos % bs) as usize;
            match extent::find(&ext, lblk) {
                Some(e) => {
                    let last_lblk = ((off + n as u64 - 1) / bs) as u64;
                    let run_end = e.end().min(last_lblk + 1);
                    let run_blocks = run_end - lblk as u64;
                    let span = ((run_blocks * bs) as usize - boff).min(n - done);
                    if e.uninit {
                        buf[done..done + span].fill(0);
                    } else {
                        let data = self.read_blocks(e.pblk + (lblk - e.lblk) as u64, run_blocks)?;
                        buf[done..done + span].copy_from_slice(&data[boff..boff + span]);
                    }
                    done += span;
                }
                None => {
                    // A hole reads as zeros, up to the next extent.
                    let next = ext.iter().find(|e| e.lblk > lblk).map(|e| e.lblk as u64 * bs).unwrap_or(u64::MAX);
                    let span = ((next - pos) as usize).min(n - done);
                    buf[done..done + span].fill(0);
                    done += span;
                }
            }
        }
        Ok(n)
    }

    fn read_whole(&mut self, inode: &Inode) -> FsResult<Vec<u8>> {
        let size = inode.size();
        if size > (1 << 31) {
            return Err(FsError::TooBig);
        }
        let mut v = alloc::vec![0u8; size as usize];
        let n = self.read_data(inode, 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    fn link_target(&mut self, inode: &Inode) -> FsResult<Vec<u8>> {
        let size = inode.size() as usize;
        if !inode.has_flag(INODE_EXTENTS) && inode.blocks_raw() == 0 && size < 60 {
            return Ok(inode.i_block()[..size].to_vec()); // fast symlink
        }
        self.read_whole(inode)
    }

    // --- path resolution ---------------------------------------------------------

    /// Look `path` up from the root. Symlinks are followed in every
    /// component, and in the last one only if `follow_last`.
    pub(crate) fn resolve(&mut self, path: &str, follow_last: bool) -> FsResult<Inode> {
        let mut queue: VecDeque<String> =
            path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        let mut cur = self.load_inode(ROOT_INO)?;
        let mut hops = 0;
        while let Some(comp) = queue.pop_front() {
            if comp == "." {
                continue;
            }
            if !cur.is_dir() {
                return Err(FsError::NotADirectory);
            }
            if comp == ".." {
                let p = self.dir_parent(&cur)?;
                cur = self.load_inode(p)?;
                continue;
            }
            let e = self.dir_lookup(&cur, comp.as_bytes())?.ok_or(FsError::NotFound)?;
            let child = self.load_inode(e.ino)?;
            if child.is_symlink() && (!queue.is_empty() || follow_last) {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(FsError::TooManyLinks);
                }
                let target = self.link_target(&child)?;
                let target = String::from_utf8(target).map_err(|_| FsError::Corrupt)?;
                if target.starts_with('/') {
                    cur = self.load_inode(ROOT_INO)?;
                }
                for c in target.split('/').rev().filter(|c| !c.is_empty()) {
                    queue.push_front(String::from(c));
                }
                continue;
            }
            cur = child;
        }
        Ok(cur)
    }

    /// Split `path` into (the directory it's in, the final name).
    fn resolve_parent(&mut self, path: &str) -> FsResult<(Inode, Vec<u8>)> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let (name, dirs) = comps.split_last().ok_or(FsError::Invalid)?;
        if *name == "." || *name == ".." {
            return Err(FsError::Invalid);
        }
        valid_name(name.as_bytes())?;
        let mut joined = String::new();
        for d in dirs {
            joined.push('/');
            joined.push_str(d);
        }
        let parent = self.resolve(&joined, true)?;
        if !parent.is_dir() {
            return Err(FsError::NotADirectory);
        }
        Ok((parent, name.as_bytes().to_vec()))
    }

    // --- creating inodes ---------------------------------------------------------

    fn new_inode(&mut self, parent: &Inode, mode: u16) -> FsResult<Inode> {
        let is_dir = mode & S_IFMT == S_IFDIR;
        let ino = self.alloc_inode(self.inode_group(parent.ino), is_dir)?;
        let mut inode = Inode::zeroed(ino, self.inode_size);
        let now = self.now();
        inode.set_mode(mode);
        inode.set_links(if is_dir { 2 } else { 1 });
        inode.set_atime(now);
        inode.set_ctime(now);
        inode.set_mtime(now);
        inode.set_generation((now as u32) ^ ino.wrapping_mul(0x9E37_79B1));
        if self.inode_size > 128 {
            let want = le16(&self.sb, 0x15E).max(32).min((self.inode_size - 128) as u16);
            put16(&mut inode.raw, 0x80, want);
            inode.set_crtime(now);
        }
        if self.incompat & INCOMPAT_EXTENTS != 0 {
            inode.set_flags(INODE_EXTENTS);
            let ib = inode.i_block_mut();
            put16(ib, 0, EXT_MAGIC);
            put16(ib, 4, 4);
        }
        Ok(inode)
    }

    fn add_link_to_dir(&mut self, parent: &mut Inode, delta: i32) -> FsResult<()> {
        let dir_nlink = self.ro_compat & RO_COMPAT_DIR_NLINK != 0;
        let cur = parent.links();
        if cur == 1 && dir_nlink {
            return Ok(()); // already "unknown": stays that way
        }
        let new = cur as i32 + delta;
        if new > LINK_MAX as i32 {
            if dir_nlink {
                parent.set_links(1);
                return Ok(());
            }
            return Err(FsError::TooBig);
        }
        parent.set_links(new.max(0) as u16);
        Ok(())
    }

    // --- writing file data -------------------------------------------------------

    /// Write `data` at `off`, allocating blocks for any holes it covers.
    /// Updates size/times and stores the inode.
    pub(crate) fn write_data(&mut self, inode: &mut Inode, off: u64, data: &[u8]) -> FsResult<()> {
        self.ensure_writable()?;
        if data.is_empty() {
            return Ok(());
        }
        self.require_extents(inode)?;
        let bs = self.bs as u64;
        let end = off.checked_add(data.len() as u64).ok_or(FsError::TooBig)?;
        if end.div_ceil(bs) > u32::MAX as u64 {
            return Err(FsError::TooBig);
        }
        let first_l = (off / bs) as u32;
        let last_l = ((end - 1) / bs) as u32;
        let mut ext = self.extents_of(inode)?;

        // 1. The holes inside [first_l, last_l].
        let mut holes: Vec<(u32, u32)> = Vec::new();
        let mut cur = first_l as u64;
        for e in &ext {
            if e.end() <= cur {
                continue;
            }
            if e.lblk as u64 > last_l as u64 {
                break;
            }
            if e.lblk as u64 > cur {
                holes.push((cur as u32, (e.lblk as u64 - cur) as u32));
            }
            cur = e.end();
        }
        if cur <= last_l as u64 {
            holes.push((cur as u32, (last_l as u64 - cur + 1) as u32));
        }

        // 2. Allocate them all up front; on failure give everything back.
        let mut goal = ext
            .iter()
            .rev()
            .find(|e| e.lblk <= first_l)
            .map(|e| e.pblk + e.len as u64)
            .unwrap_or_else(|| self.inode_goal(inode.ino));
        let mut added: Vec<Extent> = Vec::new();
        let mut fresh: Vec<(u32, u32)> = Vec::new(); // blocks whose old content is irrelevant
        for &(start, count) in &holes {
            match self.alloc_blocks(goal, count) {
                Ok(runs) => {
                    let mut l = start;
                    for (p, n) in runs {
                        added.push(Extent { lblk: l, len: n, pblk: p, uninit: false });
                        l += n;
                        goal = p + n as u64;
                    }
                    fresh.push((start, count));
                }
                Err(e) => {
                    for a in added {
                        let _ = self.free_run(a.pblk, a.len);
                    }
                    return Err(e);
                }
            }
        }

        // 3. Unwritten (preallocated) extents that this write touches
        //    become ordinary ones; their blocks must read as zeros first.
        for e in ext.iter_mut() {
            if e.uninit && e.lblk <= last_l && e.end() > first_l as u64 {
                let zeros = alloc::vec![0u8; self.bs * 64];
                let mut done = 0u64;
                while done < e.len as u64 {
                    let chunk = (e.len as u64 - done).min(64);
                    self.write_blocks(e.pblk + done, &zeros[..chunk as usize * self.bs])?;
                    done += chunk;
                }
                e.uninit = false;
                fresh.push((e.lblk, e.len));
            }
        }

        // 4. New mapping.
        ext.extend(added);
        ext.sort_by_key(|e| e.lblk);
        self.set_extents(inode, &ext)?;
        let ext = self.extents_of(inode)?;
        let is_fresh = |l: u32| fresh.iter().any(|&(s, n)| l >= s && (l as u64) < s as u64 + n as u64);

        // 5. The data itself, one physical run at a time.
        for e in &ext {
            if e.end() <= first_l as u64 || e.lblk > last_l {
                continue;
            }
            let a = e.lblk.max(first_l);
            let b = ((e.end() - 1) as u32).min(last_l);
            let nblk = (b - a + 1) as usize;
            let mut buf = alloc::vec![0u8; nblk * self.bs];
            let phys = |l: u32| e.pblk + (l - e.lblk) as u64;
            if a == first_l && off % bs != 0 && !is_fresh(a) {
                let blk = self.read_block(phys(a))?;
                buf[..self.bs].copy_from_slice(&blk);
            }
            if b == last_l && end % bs != 0 && !is_fresh(b) && !(nblk == 1 && a == first_l && off % bs != 0) {
                let blk = self.read_block(phys(b))?;
                let o = (nblk - 1) * self.bs;
                buf[o..o + self.bs].copy_from_slice(&blk);
            }
            let lo = off.max(a as u64 * bs);
            let hi = end.min((b as u64 + 1) * bs);
            buf[(lo - a as u64 * bs) as usize..(hi - a as u64 * bs) as usize]
                .copy_from_slice(&data[(lo - off) as usize..(hi - off) as usize]);
            self.write_blocks(phys(a), &buf)?;
        }

        if end > inode.size() {
            inode.set_size(end);
        }
        let now = self.now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.store_inode(inode)
    }

    /// Release every data and index block of `inode` (leaves the inode
    /// itself for the caller).
    fn free_all_blocks(&mut self, inode: &mut Inode) -> FsResult<()> {
        if !inode.has_flag(INODE_EXTENTS) && inode.blocks_raw() != 0 && !self.is_fast_symlink(inode) {
            return Err(FsError::Unsupported);
        }
        if inode.has_flag(INODE_EXTENTS) {
            let ext = self.extents_of(inode)?;
            let nodes = self.extent_node_blocks(inode)?;
            for e in &ext {
                self.free_run(e.pblk, e.len)?;
            }
            for n in nodes {
                self.free_run(n, 1)?;
            }
            self.ext_cache.remove(&inode.ino);
            let ib = inode.i_block_mut();
            ib.fill(0);
            put16(ib, 0, EXT_MAGIC);
            put16(ib, 4, 4);
            let acl = if inode.file_acl() != 0 { 1 } else { 0 };
            self.set_inode_block_count(inode, acl);
        }
        Ok(())
    }

    fn is_fast_symlink(&self, inode: &Inode) -> bool {
        inode.is_symlink() && !inode.has_flag(INODE_EXTENTS) && inode.blocks_raw() == 0 && inode.size() < 60
    }

    pub(crate) fn truncate_data(&mut self, inode: &mut Inode, len: u64) -> FsResult<()> {
        self.ensure_writable()?;
        let size = inode.size();
        let now = self.now();
        if len < size {
            self.require_extents(inode)?;
            let bs = self.bs as u64;
            let keep = len.div_ceil(bs);
            let ext = self.extents_of(inode)?;
            let mut kept: Vec<Extent> = Vec::new();
            let mut freed: Vec<(u64, u32)> = Vec::new();
            for e in ext {
                if e.lblk as u64 >= keep {
                    freed.push((e.pblk, e.len));
                } else if e.end() > keep {
                    let k = (keep - e.lblk as u64) as u32;
                    kept.push(Extent { len: k, ..e });
                    freed.push((e.pblk + k as u64, e.len - k));
                } else {
                    kept.push(e);
                }
            }
            // Shrink the mapping first, then release the blocks: a crash
            // in between leaks blocks (fsck reclaims) rather than leaving
            // a file pointing at freed ones.
            inode.set_size(len);
            self.set_extents(inode, &kept)?;
            self.store_inode(inode)?;
            for (p, n) in freed {
                self.free_run(p, n)?;
            }
            // Blank the tail of the (now last) partial block so stale
            // bytes can't reappear if the file grows again.
            if len % bs != 0 {
                if let Some(e) = extent::find(&kept, (len / bs) as u32) {
                    if !e.uninit {
                        let pblk = e.pblk + (len / bs - e.lblk as u64);
                        let mut b = self.read_block(pblk)?;
                        b[(len % bs) as usize..].fill(0);
                        self.write_block(pblk, &b)?;
                    }
                }
            }
        }
        inode.set_size(len);
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.store_inode(inode)
    }

    // --- destroying inodes -------------------------------------------------------

    /// Drop this inode's reference to its extended-attribute block.
    fn release_xattr_block(&mut self, inode: &Inode) -> FsResult<()> {
        let acl = inode.file_acl();
        if acl == 0 || acl >= self.blocks_count {
            return Ok(());
        }
        let mut b = self.read_block(acl)?;
        let refcount = le32(&b, 4);
        if le32(&b, 0) != 0xEA02_0000 {
            return Ok(()); // not what we think it is: leave it alone
        }
        if refcount > 1 {
            put32(&mut b, 4, refcount - 1);
            if self.has_metadata_csum() {
                put32(&mut b, 0x10, 0);
                let c = crc32c(crc32c(self.csum_seed, &acl.to_le_bytes()), &b);
                put32(&mut b, 0x10, c);
            }
            self.write_block(acl, &b)
        } else {
            self.free_run(acl, 1)
        }
    }

    /// Free an unlinked inode: its blocks, then the inode itself.
    fn destroy_inode(&mut self, mut inode: Inode) -> FsResult<()> {
        let was_dir = inode.is_dir();
        self.release_xattr_block(&inode)?;
        self.free_all_blocks(&mut inode)?;
        let now = self.now() as u32;
        inode.set_dtime(now);
        inode.set_links(0);
        inode.set_size(0);
        self.set_inode_block_count(&mut inode, 0);
        put32(&mut inode.raw, 0x68, 0); // file_acl
        if !inode.has_flag(INODE_EXTENTS) {
            inode.i_block_mut().fill(0);
        }
        self.store_inode(&inode)?;
        self.free_inode_number(inode.ino, was_dir)
    }

    /// Drop one link; free the inode once nothing (names or open
    /// handles) refers to it.
    fn drop_link(&mut self, mut inode: Inode) -> FsResult<()> {
        let now = self.now();
        let links = inode.links().saturating_sub(1);
        inode.set_links(links);
        inode.set_ctime(now);
        if links == 0 {
            if self.pinned.get(&inode.ino).copied().unwrap_or(0) > 0 {
                self.orphans.push(inode.ino);
                return self.store_inode(&inode);
            }
            return self.destroy_inode(inode);
        }
        self.store_inode(&inode)
    }

    // --- namespace operations ------------------------------------------------------

    fn op_create_file(&mut self, parent: &mut Inode, name: &[u8]) -> FsResult<Inode> {
        let mut inode = self.new_inode(parent, S_IFREG | 0o644)?;
        self.store_inode(&inode)?;
        if let Err(e) = self.dir_add(parent, name, inode.ino, FT_REG) {
            inode.set_links(0);
            let _ = self.destroy_inode(inode);
            return Err(e);
        }
        Ok(inode)
    }

    fn op_mkdir(&mut self, path: &str) -> FsResult<()> {
        let (mut parent, name) = self.resolve_parent(path)?;
        if self.dir_lookup(&parent, &name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let mut dir = self.new_inode(&parent, S_IFDIR | 0o755)?;
        let goal = self.inode_goal(parent.ino);
        let pblk = match self.alloc_run(goal, 1) {
            Ok((b, _)) => b,
            Err(e) => {
                let _ = self.destroy_inode(dir);
                return Err(e);
            }
        };
        let ext = [Extent { lblk: 0, len: 1, pblk, uninit: false }];
        self.set_extents(&mut dir, &ext)?;
        dir.set_size(self.bs as u64);
        self.store_inode(&dir)?;
        self.dir_write_new(&dir, pblk, parent.ino)?;
        self.dir_add(&mut parent, &name, dir.ino, FT_DIR)?;
        self.add_link_to_dir(&mut parent, 1)?;
        self.store_inode(&parent)
    }

    fn op_unlink(&mut self, path: &str) -> FsResult<()> {
        let (mut parent, name) = self.resolve_parent(path)?;
        let e = self.dir_lookup(&parent, &name)?.ok_or(FsError::NotFound)?;
        let inode = self.load_inode(e.ino)?;
        if inode.is_dir() {
            return Err(FsError::NotAFile);
        }
        self.dir_remove(&mut parent, &name)?;
        self.drop_link(inode)
    }

    fn op_rmdir(&mut self, path: &str) -> FsResult<()> {
        let (mut parent, name) = self.resolve_parent(path)?;
        let e = self.dir_lookup(&parent, &name)?.ok_or(FsError::NotFound)?;
        let inode = self.load_inode(e.ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotADirectory);
        }
        if !self.dir_is_empty(&inode)? {
            return Err(FsError::NotEmpty);
        }
        self.dir_remove(&mut parent, &name)?;
        self.add_link_to_dir(&mut parent, -1)?;
        self.store_inode(&parent)?;
        self.destroy_inode(inode)
    }

    fn op_rename(&mut self, from: &str, to: &str) -> FsResult<()> {
        let (mut sp, sname) = self.resolve_parent(from)?;
        let (mut dp, dname) = self.resolve_parent(to)?;
        let se = self.dir_lookup(&sp, &sname)?.ok_or(FsError::NotFound)?;
        let src = self.load_inode(se.ino)?;

        // A directory can't move into its own subtree.
        if src.is_dir() {
            let mut cur = dp.ino;
            loop {
                if cur == src.ino {
                    return Err(FsError::Invalid);
                }
                if cur == ROOT_INO {
                    break;
                }
                let d = self.load_inode(cur)?;
                let up = self.dir_parent(&d)?;
                if up == cur {
                    break;
                }
                cur = up;
            }
        }
        if sp.ino == dp.ino && sname == dname {
            return Ok(());
        }
        if let Some(de) = self.dir_lookup(&dp, &dname)? {
            if de.ino == src.ino {
                return Ok(()); // two names for one file: POSIX says do nothing
            }
            let dst = self.load_inode(de.ino)?;
            if dst.is_dir() != src.is_dir() {
                return Err(if dst.is_dir() { FsError::AlreadyExists } else { FsError::NotADirectory });
            }
            if dst.is_dir() && !self.dir_is_empty(&dst)? {
                return Err(FsError::NotEmpty);
            }
            self.dir_remove(&mut dp, &dname)?;
            if dst.is_dir() {
                self.add_link_to_dir(&mut dp, -1)?;
                self.store_inode(&dp)?;
                self.destroy_inode(dst)?;
            } else {
                self.drop_link(dst)?;
            }
            if sp.ino == dp.ino {
                sp = self.load_inode(sp.ino)?;
                dp = sp.clone();
            }
        }

        let ftype = Self::ftype_of(&src);
        self.dir_add(&mut dp, &dname, src.ino, ftype)?;
        if sp.ino == dp.ino {
            sp = dp.clone();
        }
        self.dir_remove(&mut sp, &sname)?;
        if src.is_dir() && sp.ino != dp.ino {
            let mut moved = self.load_inode(src.ino)?;
            self.dir_set_parent(&mut moved, dp.ino)?;
            let mut sp2 = self.load_inode(sp.ino)?;
            self.add_link_to_dir(&mut sp2, -1)?;
            self.store_inode(&sp2)?;
            let mut dp2 = self.load_inode(dp.ino)?;
            self.add_link_to_dir(&mut dp2, 1)?;
            self.store_inode(&dp2)?;
        }
        let mut moved = self.load_inode(src.ino)?;
        moved.set_ctime(self.now());
        self.store_inode(&moved)
    }

    fn op_hard_link(&mut self, existing: &str, new: &str) -> FsResult<()> {
        let inode = self.resolve(existing, false)?;
        if inode.is_dir() {
            return Err(FsError::NotAFile); // no hard links to directories
        }
        if inode.links() >= LINK_MAX {
            return Err(FsError::TooBig);
        }
        let (mut parent, name) = self.resolve_parent(new)?;
        if self.dir_lookup(&parent, &name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        self.dir_add(&mut parent, &name, inode.ino, Self::ftype_of(&inode))?;
        let mut inode = self.load_inode(inode.ino)?;
        inode.set_links(inode.links() + 1);
        inode.set_ctime(self.now());
        self.store_inode(&inode)
    }

    fn op_symlink(&mut self, target: &str, link: &str) -> FsResult<()> {
        let t = target.as_bytes();
        if t.is_empty() || t.len() >= self.bs {
            return Err(FsError::Invalid);
        }
        let (mut parent, name) = self.resolve_parent(link)?;
        if self.dir_lookup(&parent, &name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let mut inode = self.new_inode(&parent, S_IFLNK | 0o777)?;
        if t.len() < 60 {
            // Fast symlink: the target lives in i_block; no extents.
            inode.set_flags(inode.flags() & !INODE_EXTENTS);
            let ib = inode.i_block_mut();
            ib.fill(0);
            ib[..t.len()].copy_from_slice(t);
            inode.set_size(t.len() as u64);
            self.store_inode(&inode)?;
        } else {
            let goal = self.inode_goal(parent.ino);
            let (pblk, _) = match self.alloc_run(goal, 1) {
                Ok(r) => r,
                Err(e) => {
                    let _ = self.destroy_inode(inode);
                    return Err(e);
                }
            };
            let mut block = alloc::vec![0u8; self.bs];
            block[..t.len()].copy_from_slice(t);
            self.write_block(pblk, &block)?;
            self.set_extents(&mut inode, &[Extent { lblk: 0, len: 1, pblk, uninit: false }])?;
            inode.set_size(t.len() as u64);
            self.store_inode(&inode)?;
        }
        if let Err(e) = self.dir_add(&mut parent, &name, inode.ino, FT_SYMLINK) {
            inode.set_links(0);
            let _ = self.destroy_inode(inode);
            return Err(e);
        }
        Ok(())
    }

    /// Open (optionally creating/truncating) the regular file at `path`.
    fn op_open(&mut self, path: &str, create: bool, truncate: bool) -> FsResult<u32> {
        let mut inode = match self.resolve(path, true) {
            Ok(i) => i,
            Err(FsError::NotFound) if create => {
                self.ensure_writable()?;
                let (mut parent, name) = self.resolve_parent(path)?;
                // A dangling symlink at `path` would have resolved above
                // to NotFound too; creating through it isn't supported.
                if let Some(e) = self.dir_lookup(&parent, &name)? {
                    let _ = e;
                    return Err(FsError::NotFound);
                }
                self.op_create_file(&mut parent, &name)?
            }
            Err(e) => return Err(e),
        };
        if inode.is_dir() {
            return Err(FsError::NotAFile);
        }
        if !inode.is_reg() {
            return Err(FsError::Unsupported); // devices, fifos, sockets
        }
        if truncate && inode.size() > 0 {
            self.ensure_writable()?;
            self.truncate_data(&mut inode, 0)?;
        }
        *self.pinned.entry(inode.ino).or_insert(0) += 1;
        Ok(inode.ino)
    }
}

impl FileSystem for Ext4Fs {
    fn fs_type(&self) -> &'static str {
        "ext4"
    }

    fn label(&self) -> String {
        self.label_string()
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn note(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.replayed > 0 {
            parts.push(alloc::format!("replayed {} journal transaction(s)", self.replayed));
        }
        if let Some(why) = self.read_only_reason {
            parts.push(alloc::format!("read-only: {}", why));
        }
        if parts.is_empty() { None } else { Some(parts.join("; ")) }
    }

    fn space(&mut self) -> (u64, u64) {
        let bs = self.bs as u64;
        (self.blocks_count * bs, self.free_blocks_total() * bs)
    }

    fn read_dir_at(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        let dir = self.resolve(path, true)?;
        if !dir.is_dir() {
            return Err(FsError::NotADirectory);
        }
        let mut out = Vec::new();
        for e in self.dir_list(&dir)? {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let inode = self.load_inode(e.ino)?;
            let name = String::from_utf8_lossy(&e.name).into_owned();
            out.push(self.entry_of(&name, &inode));
        }
        Ok(out)
    }

    fn find(&mut self, path: &str) -> FsResult<DirEntry> {
        let inode = self.resolve(path, true)?;
        let name = path.rsplit('/').find(|c| !c.is_empty()).unwrap_or("");
        Ok(self.entry_of(name, &inode))
    }

    fn find_nofollow(&mut self, path: &str) -> FsResult<DirEntry> {
        let inode = self.resolve(path, false)?;
        let name = path.rsplit('/').find(|c| !c.is_empty()).unwrap_or("");
        Ok(self.entry_of(name, &inode))
    }

    fn read_file(&mut self, entry: &DirEntry) -> FsResult<Vec<u8>> {
        let inode = self.load_inode(entry.first_cluster)?;
        if inode.is_dir() {
            return Err(FsError::NotAFile);
        }
        self.read_whole(&inode)
    }

    fn write_file(&mut self, path: &str, data: &[u8]) -> FsResult<()> {
        self.mutate(|s| {
            let ino = s.op_open(path, true, true)?;
            let r = (|| {
                let mut inode = s.load_inode(ino)?;
                s.write_data(&mut inode, 0, data)
            })();
            s.op_release(ino);
            r
        })
    }

    fn delete_file(&mut self, path: &str) -> FsResult<()> {
        self.mutate(|s| s.op_unlink(path))
    }

    fn create_dir(&mut self, path: &str) -> FsResult<()> {
        self.mutate(|s| s.op_mkdir(path))
    }

    fn remove_dir(&mut self, path: &str) -> FsResult<()> {
        self.mutate(|s| s.op_rmdir(path))
    }

    fn rename(&mut self, from: &str, to: &str) -> FsResult<()> {
        self.mutate(|s| s.op_rename(from, to))
    }

    fn read_link(&mut self, path: &str) -> FsResult<String> {
        let inode = self.resolve(path, false)?;
        if !inode.is_symlink() {
            return Err(FsError::Invalid);
        }
        let t = self.link_target(&inode)?;
        String::from_utf8(t).map_err(|_| FsError::Corrupt)
    }

    fn symlink(&mut self, target: &str, link: &str) -> FsResult<()> {
        self.mutate(|s| s.op_symlink(target, link))
    }

    fn hard_link(&mut self, existing: &str, new: &str) -> FsResult<()> {
        self.mutate(|s| s.op_hard_link(existing, new))
    }

    fn set_perm(&mut self, path: &str, perm: u32) -> FsResult<()> {
        self.mutate(|s| {
            let mut inode = s.resolve(path, true)?;
            let mode = (inode.mode() & S_IFMT) | (perm & 0o7777) as u16;
            inode.set_mode(mode);
            inode.set_ctime(s.now());
            s.store_inode(&inode)
        })
    }

    fn open_inode(&mut self, path: &str, create: bool, truncate: bool) -> FsResult<Option<u64>> {
        if create || truncate {
            self.ensure_writable()?;
        }
        if create || truncate {
            self.mutate(|s| s.op_open(path, create, truncate)).map(|i| Some(i as u64))
        } else {
            self.op_open(path, false, false).map(|i| Some(i as u64))
        }
    }

    fn read_inode(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        let inode = self.load_inode(ino as u32)?;
        self.read_data(&inode, offset, buf)
    }

    fn write_inode(&mut self, ino: u64, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.mutate(|s| {
            let mut inode = s.load_inode(ino as u32)?;
            s.write_data(&mut inode, offset, data)?;
            Ok(data.len())
        })
    }

    fn truncate_inode(&mut self, ino: u64, len: u64) -> FsResult<()> {
        self.mutate(|s| {
            let mut inode = s.load_inode(ino as u32)?;
            s.truncate_data(&mut inode, len)
        })
    }

    fn size_inode(&mut self, ino: u64) -> FsResult<u64> {
        Ok(self.load_inode(ino as u32)?.size())
    }

    fn release_inode(&mut self, ino: u64) {
        self.op_release(ino as u32);
    }

    fn sync(&mut self) -> FsResult<()> {
        if self.read_only {
            return Ok(());
        }
        self.flush_sb()
    }
}

impl Ext4Fs {
    /// Undo one `op_open` pin; if it was the last one on an unlinked
    /// inode, free it now.
    fn op_release(&mut self, ino: u32) {
        let left = match self.pinned.get_mut(&ino) {
            Some(n) => {
                *n = n.saturating_sub(1);
                *n
            }
            None => return,
        };
        if left > 0 {
            return;
        }
        self.pinned.remove(&ino);
        if let Some(pos) = self.orphans.iter().position(|&o| o == ino) {
            self.orphans.remove(pos);
            if let Ok(inode) = self.load_inode(ino) {
                let _ = self.destroy_inode(inode);
                let _ = self.flush_sb();
            }
        }
    }
}
