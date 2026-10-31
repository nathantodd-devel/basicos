//! Minimal FAT32 filesystem driver: mount, list directories, read and
//! write files by 8.3 name.
//!
//! Deliberate limitations, to keep this a weekend-sized driver instead
//! of a whole subsystem:
//! - No long file names (LFN entries are parsed-past, not decoded) —
//!   names are limited to classic 8.3 (`HELLO.TXT`).
//! - No partition table support — the volume must start at block 0 of
//!   the device (a "superfloppy" layout), which is exactly what
//!   `mkfs.vfat -F32 <raw file>` produces without `fdisk` involved.
//! - No `mkdir`: `write_file` can create files in an existing directory
//!   (including root) but can't create new subdirectories.
//! - FAT is updated synchronously on every allocation/free — correct,
//!   but not batched, so heavy write workloads will be slow. Fine for a
//!   hobby OS.
//!
//! Reference: Microsoft's "FAT32 File System Specification".

use super::block::{BlockDevice, BlockError, BLOCK_SIZE};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub use crate::vfs::FsError as Fat32Error;
use crate::vfs::{Attr, FileSystem, FsResult};

impl From<BlockError> for Fat32Error {
    fn from(_: BlockError) -> Self {
        Fat32Error::Io
    }
}

fn le_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn le_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

// --- on-disk layout --------------------------------------------------------

/// Fields we care about from the BIOS Parameter Block (boot sector).
#[derive(Debug, Clone, Copy)]
struct Bpb {
    num_fats: u8,
    reserved_sectors: u16,
    fat_size: u32,
    total_sectors: u32,
    root_cluster: u32,
}

impl Bpb {
    fn parse(sector0: &[u8; BLOCK_SIZE]) -> Result<Self, Fat32Error> {
        // Boot sector signature.
        if le_u16(sector0, 510) != 0xAA55 {
            return Err(Fat32Error::NotFat32);
        }
        let bytes_per_sector = le_u16(sector0, 11);
        if bytes_per_sector as usize != BLOCK_SIZE {
            return Err(Fat32Error::NotFat32); // we only speak 512-byte sectors
        }

        let fat_size_16 = le_u16(sector0, 22);
        if fat_size_16 != 0 {
            // FAT12/16 store a nonzero sectors-per-FAT here; FAT32 always
            // sets it to 0 and uses the 32-bit field at offset 36 instead.
            // That makes this the standard way to tell FAT32 apart from
            // its predecessors.
            return Err(Fat32Error::NotFat32);
        }

        let num_fats = sector0[16];
        let reserved_sectors = le_u16(sector0, 14);
        let total_sectors_16 = le_u16(sector0, 19);
        let total_sectors_32 = le_u32(sector0, 32);
        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16 as u32
        } else {
            total_sectors_32
        };
        let fat_size = le_u32(sector0, 36);
        let root_cluster = le_u32(sector0, 44);

        if num_fats == 0 || fat_size == 0 || root_cluster < 2 {
            return Err(Fat32Error::NotFat32);
        }

        Ok(Bpb {
            num_fats,
            reserved_sectors,
            fat_size,
            total_sectors,
            root_cluster,
        })
    }
}

/// Geometry derived from the BPB, so we're not repeating arithmetic
/// (and misplacing a `+1`) all over the driver.
#[derive(Debug, Clone, Copy)]
struct Layout {
    bpb: Bpb,
    first_fat_sector: u32,
    first_data_sector: u32,
    sectors_per_cluster: u32,
    cluster_size: usize,
}

impl Layout {
    fn from_bpb(bpb: Bpb, sectors_per_cluster: u8) -> Self {
        let first_fat_sector = bpb.reserved_sectors as u32;
        let first_data_sector = first_fat_sector + bpb.num_fats as u32 * bpb.fat_size;
        let sectors_per_cluster = sectors_per_cluster as u32;
        Self {
            bpb,
            first_fat_sector,
            first_data_sector,
            sectors_per_cluster,
            cluster_size: sectors_per_cluster as usize * BLOCK_SIZE,
        }
    }

    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.first_data_sector + (cluster - 2) * self.sectors_per_cluster
    }

    fn total_clusters(&self) -> u32 {
        self.bpb.total_sectors / self.sectors_per_cluster.max(1)
    }
}

// FAT32 entries are 28 significant bits; the top 4 are reserved.
const FAT32_ENTRY_MASK: u32 = 0x0FFF_FFFF;
const FAT32_FREE_CLUSTER: u32 = 0x0000_0000;
const FAT32_EOC: u32 = 0x0FFF_FFF8; // "end of chain" — anything >= this
const FAT32_BAD_CLUSTER: u32 = 0x0FFF_FFF7;

// Directory entry attribute bits.
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LFN: u8 = 0x0F; // READ_ONLY|HIDDEN|SYSTEM|VOLUME_ID all set

pub use crate::vfs::DirEntry;

fn fat_short_name_to_string(name: &[u8], ext: &[u8]) -> String {
    let mut s = String::new();
    for &b in name {
        if b == b' ' {
            break;
        }
        s.push(b as char);
    }
    let ext_len = ext.iter().take_while(|&&b| b != b' ').count();
    if ext_len > 0 {
        s.push('.');
        for &b in &ext[..ext_len] {
            s.push(b as char);
        }
    }
    s
}

/// Convert `"hello.txt"` into a raw, padded 8.3 short name
/// (`"HELLO   TXT"`, 11 bytes, no dot). Names that don't fit 8+3
/// characters are rejected outright rather than silently truncated —
/// there's no long-name support to fall back on here.
fn to_short_name(name: &str) -> Result<[u8; 11], Fat32Error> {
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !name.is_ascii() {
        return Err(Fat32Error::NameTooLong);
    }
    let mut short = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        short[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        short[8 + i] = b.to_ascii_uppercase();
    }
    Ok(short)
}

/// Split `"dir/sub/file.txt"` into `("dir/sub", "file.txt")`; a bare
/// `"file.txt"` splits into `("", "file.txt")`.
fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_matches('/');
    match trimmed.rfind('/') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("", trimmed),
    }
}

/// A mounted FAT32 volume.
pub struct Fat32Fs {
    device: Box<dyn BlockDevice>,
    layout: Layout,
}

impl Fat32Fs {
    /// Parse the boot sector and mount `device` as a FAT32 volume.
    pub fn mount(mut device: Box<dyn BlockDevice>) -> Result<Self, Fat32Error> {
        let mut sector0 = [0u8; BLOCK_SIZE];
        device.read_block(0, &mut sector0)?;
        let bpb = Bpb::parse(&sector0)?;
        let sectors_per_cluster = sector0[13];
        if sectors_per_cluster == 0 {
            return Err(Fat32Error::NotFat32);
        }
        Ok(Self {
            device,
            layout: Layout::from_bpb(bpb, sectors_per_cluster),
        })
    }

    pub fn root_cluster(&self) -> u32 {
        self.layout.bpb.root_cluster
    }

    // --- FAT table access ---------------------------------------------

    fn read_fat_entry(&mut self, cluster: u32) -> Result<u32, Fat32Error> {
        let fat_byte_offset = cluster as u64 * 4;
        let sector = self.layout.first_fat_sector as u64 + fat_byte_offset / BLOCK_SIZE as u64;
        let offset_in_sector = (fat_byte_offset % BLOCK_SIZE as u64) as usize;
        let mut buf = [0u8; BLOCK_SIZE];
        self.device.read_block(sector, &mut buf)?;
        Ok(le_u32(&buf, offset_in_sector) & FAT32_ENTRY_MASK)
    }

    /// Write one FAT entry, mirrored across every FAT copy (`num_fats`,
    /// almost always 2) like a real FAT32 driver should.
    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), Fat32Error> {
        let fat_byte_offset = cluster as u64 * 4;
        let sector_in_fat = fat_byte_offset / BLOCK_SIZE as u64;
        let offset_in_sector = (fat_byte_offset % BLOCK_SIZE as u64) as usize;

        for fat_index in 0..self.layout.bpb.num_fats as u64 {
            let sector = self.layout.first_fat_sector as u64
                + fat_index * self.layout.bpb.fat_size as u64
                + sector_in_fat;
            let mut buf = [0u8; BLOCK_SIZE];
            self.device.read_block(sector, &mut buf)?;
            let preserved_top_bits = le_u32(&buf, offset_in_sector) & !FAT32_ENTRY_MASK;
            let new_value = (value & FAT32_ENTRY_MASK) | preserved_top_bits;
            buf[offset_in_sector..offset_in_sector + 4].copy_from_slice(&new_value.to_le_bytes());
            self.device.write_block(sector, &buf)?;
        }
        Ok(())
    }

    /// Follow a cluster chain from `start`, returning every cluster in
    /// order (not including the EOC marker).
    fn cluster_chain(&mut self, start: u32) -> Result<Vec<u32>, Fat32Error> {
        let mut chain = Vec::new();
        let mut cluster = start;
        let max_clusters = self.layout.total_clusters() + 1;
        while cluster >= 2 && cluster < FAT32_BAD_CLUSTER {
            chain.push(cluster);
            if chain.len() as u32 > max_clusters {
                return Err(Fat32Error::Io); // corrupt/looping FAT
            }
            cluster = self.read_fat_entry(cluster)?;
        }
        Ok(chain)
    }

    /// Allocate `count` free clusters, linked into a chain and
    /// terminated with an EOC marker. Doesn't touch any directory entry.
    fn alloc_clusters(&mut self, count: usize) -> Result<Vec<u32>, Fat32Error> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let total_clusters = self.layout.total_clusters();
        let mut found = Vec::with_capacity(count);
        let mut cluster = 2u32;
        while found.len() < count && cluster < total_clusters {
            if self.read_fat_entry(cluster)? == FAT32_FREE_CLUSTER {
                found.push(cluster);
            }
            cluster += 1;
        }
        if found.len() < count {
            return Err(Fat32Error::NoSpace);
        }
        for i in 0..found.len() {
            let value = if i + 1 < found.len() {
                found[i + 1]
            } else {
                FAT32_EOC
            };
            self.write_fat_entry(found[i], value)?;
        }
        Ok(found)
    }

    /// Free every cluster in the chain starting at `start`.
    fn free_chain(&mut self, start: u32) -> Result<(), Fat32Error> {
        let chain = self.cluster_chain(start)?;
        for c in chain {
            self.write_fat_entry(c, FAT32_FREE_CLUSTER)?;
        }
        Ok(())
    }

    // --- cluster data ----------------------------------------------------

    fn read_cluster(&mut self, cluster: u32) -> Result<Vec<u8>, Fat32Error> {
        let mut data = vec![0u8; self.layout.cluster_size];
        let start_sector = self.layout.cluster_to_sector(cluster) as u64;
        for i in 0..self.layout.sectors_per_cluster as u64 {
            let off = i as usize * BLOCK_SIZE;
            self.device
                .read_block(start_sector + i, &mut data[off..off + BLOCK_SIZE])?;
        }
        Ok(data)
    }

    fn write_cluster(&mut self, cluster: u32, data: &[u8]) -> Result<(), Fat32Error> {
        debug_assert_eq!(data.len(), self.layout.cluster_size);
        let start_sector = self.layout.cluster_to_sector(cluster) as u64;
        for i in 0..self.layout.sectors_per_cluster as u64 {
            let off = i as usize * BLOCK_SIZE;
            self.device
                .write_block(start_sector + i, &data[off..off + BLOCK_SIZE])?;
        }
        Ok(())
    }

    // --- directories -------------------------------------------------------

    fn parse_dir_entries(raw: &[u8]) -> Vec<DirEntry> {
        let mut entries = Vec::new();
        for chunk in raw.chunks_exact(32) {
            match chunk[0] {
                0x00 => break,    // end of directory
                0xE5 => continue, // deleted
                _ => {}
            }
            let attr = chunk[11];
            if attr == ATTR_LFN || attr & ATTR_VOLUME_ID != 0 {
                continue;
            }
            let name = fat_short_name_to_string(&chunk[0..8], &chunk[8..11]);
            if name == "." || name == ".." {
                continue;
            }
            let hi = le_u16(chunk, 20) as u32;
            let lo = le_u16(chunk, 26) as u32;
            entries.push(DirEntry {
                name,
                is_dir: attr & ATTR_DIRECTORY != 0,
                size: le_u32(chunk, 28) as u64,
                first_cluster: (hi << 16) | lo,
                attr: Attr::default(),
            });
        }
        entries
    }

    /// List the entries of the directory starting at `cluster` (pass
    /// [`Fat32Fs::root_cluster`] for the root).
    pub fn read_dir(&mut self, cluster: u32) -> Result<Vec<DirEntry>, Fat32Error> {
        let chain = self.cluster_chain(cluster)?;
        let mut raw = Vec::with_capacity(chain.len() * self.layout.cluster_size);
        for c in chain {
            raw.extend(self.read_cluster(c)?);
        }
        Ok(Self::parse_dir_entries(&raw))
    }

    /// List the directory at `path` (`""` or `"/"` for root).
    pub fn read_dir_at(&mut self, path: &str) -> Result<Vec<DirEntry>, Fat32Error> {
        let cluster = self.resolve_dir(path)?;
        self.read_dir(cluster)
    }

    /// Resolve a `/`-separated path to a directory's first cluster.
    fn resolve_dir(&mut self, path: &str) -> Result<u32, Fat32Error> {
        if path.is_empty() || path == "/" {
            return Ok(self.root_cluster());
        }
        let entry = self.find(path)?;
        if !entry.is_dir {
            return Err(Fat32Error::NotADirectory);
        }
        Ok(entry.first_cluster)
    }

    /// Resolve a `/`-separated path to its directory entry.
    pub fn find(&mut self, path: &str) -> Result<DirEntry, Fat32Error> {
        let mut cluster = self.root_cluster();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(Fat32Error::NotFound); // "" / "/" isn't a file or a named dir
        }
        let mut found: Option<DirEntry> = None;
        for (i, part) in parts.iter().enumerate() {
            let entries = self.read_dir(cluster)?;
            let entry = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or(Fat32Error::NotFound)?;
            if i + 1 != parts.len() && !entry.is_dir {
                return Err(Fat32Error::NotADirectory);
            }
            cluster = entry.first_cluster;
            found = Some(entry);
        }
        Ok(found.expect("parts is non-empty"))
    }

    /// Find a raw entry by its exact short name within one directory,
    /// returning `(cluster holding the 32-byte slot, offset within that
    /// cluster, decoded entry)` so callers can overwrite it in place.
    fn locate_entry(
        &mut self,
        dir_cluster: u32,
        short_name: &[u8; 11],
    ) -> Result<Option<(u32, usize, DirEntry)>, Fat32Error> {
        for cluster in self.cluster_chain(dir_cluster)? {
            let raw = self.read_cluster(cluster)?;
            for (i, chunk) in raw.chunks_exact(32).enumerate() {
                if chunk[0] == 0x00 {
                    return Ok(None); // end of directory
                }
                if chunk[0] == 0xE5 || chunk[11] == ATTR_LFN {
                    continue;
                }
                if chunk[0..11] == *short_name {
                    let hi = le_u16(chunk, 20) as u32;
                    let lo = le_u16(chunk, 26) as u32;
                    let entry = DirEntry {
                        name: fat_short_name_to_string(&chunk[0..8], &chunk[8..11]),
                        is_dir: chunk[11] & ATTR_DIRECTORY != 0,
                        size: le_u32(chunk, 28) as u64,
                        first_cluster: (hi << 16) | lo,
                        attr: Attr::default(),
                    };
                    return Ok(Some((cluster, i * 32, entry)));
                }
            }
        }
        Ok(None)
    }

    /// Find a free (deleted or never-used) 32-byte directory slot,
    /// extending the directory with a fresh zeroed cluster if every
    /// existing one is full.
    fn find_free_slot(&mut self, dir_cluster: u32) -> Result<(u32, usize), Fat32Error> {
        let chain = self.cluster_chain(dir_cluster)?;
        for &cluster in &chain {
            let raw = self.read_cluster(cluster)?;
            for (i, chunk) in raw.chunks_exact(32).enumerate() {
                if chunk[0] == 0x00 || chunk[0] == 0xE5 {
                    return Ok((cluster, i * 32));
                }
            }
        }
        let last = *chain.last().ok_or(Fat32Error::Io)?;
        let new_cluster = self.alloc_clusters(1)?[0];
        self.write_fat_entry(last, new_cluster)?;
        let zeroed = vec![0u8; self.layout.cluster_size];
        self.write_cluster(new_cluster, &zeroed)?;
        Ok((new_cluster, 0))
    }

    /// Overwrite a directory entry slot's raw bytes in place.
    fn write_entry_slot(
        &mut self,
        cluster: u32,
        byte_offset: usize,
        short_name: &[u8; 11],
        is_dir: bool,
        size: u32,
        first_cluster: u32,
    ) -> Result<(), Fat32Error> {
        let mut raw = self.read_cluster(cluster)?;
        let slot = &mut raw[byte_offset..byte_offset + 32];
        slot.fill(0);
        slot[0..11].copy_from_slice(short_name);
        slot[11] = if is_dir { ATTR_DIRECTORY } else { 0x20 }; // 0x20 = archive
        slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
        slot[28..32].copy_from_slice(&size.to_le_bytes());
        self.write_cluster(cluster, &raw)
    }

    // --- files ---------------------------------------------------------

    /// Read a whole file's contents, given its directory entry.
    pub fn read_file(&mut self, entry: &DirEntry) -> Result<Vec<u8>, Fat32Error> {
        if entry.is_dir {
            return Err(Fat32Error::NotAFile);
        }
        if entry.size == 0 || entry.first_cluster < 2 {
            return Ok(Vec::new());
        }
        let chain = self.cluster_chain(entry.first_cluster)?;
        let mut data = Vec::with_capacity(entry.size as usize);
        for c in chain {
            data.extend(self.read_cluster(c)?);
        }
        data.truncate(entry.size as usize);
        Ok(data)
    }

    /// Resolve `path` and read the whole file.
    pub fn read_file_at(&mut self, path: &str) -> Result<Vec<u8>, Fat32Error> {
        let entry = self.find(path)?;
        self.read_file(&entry)
    }

    /// Create or overwrite the file at `path` with `data`. The parent
    /// directory must already exist (root counts); this never creates
    /// subdirectories.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), Fat32Error> {
        let (dir_path, file_name) = split_path(path);
        if file_name.is_empty() {
            return Err(Fat32Error::NotAFile);
        }
        let short_name = to_short_name(file_name)?;
        let dir_cluster = self.resolve_dir(dir_path)?;

        // Overwrite in place if the file already exists — free its old
        // data first so we don't leak clusters.
        let existing = self.locate_entry(dir_cluster, &short_name)?;
        if let Some((_, _, entry)) = &existing {
            if entry.is_dir {
                return Err(Fat32Error::NotAFile);
            }
            if entry.first_cluster >= 2 {
                self.free_chain(entry.first_cluster)?;
            }
        }

        let clusters_needed = if data.is_empty() {
            0
        } else {
            (data.len() + self.layout.cluster_size - 1) / self.layout.cluster_size
        };
        let chain = self.alloc_clusters(clusters_needed)?;
        for (i, &cluster) in chain.iter().enumerate() {
            let start = i * self.layout.cluster_size;
            let end = (start + self.layout.cluster_size).min(data.len());
            let mut buf = vec![0u8; self.layout.cluster_size];
            buf[..end - start].copy_from_slice(&data[start..end]);
            self.write_cluster(cluster, &buf)?;
        }
        let first_cluster = chain.first().copied().unwrap_or(0);

        let (slot_cluster, slot_offset) = match &existing {
            Some((c, o, _)) => (*c, *o),
            None => self.find_free_slot(dir_cluster)?,
        };
        self.write_entry_slot(
            slot_cluster,
            slot_offset,
            &short_name,
            false,
            data.len() as u32,
            first_cluster,
        )
    }

    /// Delete the file at `path`, freeing its cluster chain.
    pub fn delete_file(&mut self, path: &str) -> Result<(), Fat32Error> {
        let (dir_path, file_name) = split_path(path);
        if file_name.is_empty() {
            return Err(Fat32Error::NotFound);
        }
        let short_name = to_short_name(file_name)?;
        let dir_cluster = self.resolve_dir(dir_path)?;
        let (cluster, offset, entry) = self
            .locate_entry(dir_cluster, &short_name)?
            .ok_or(Fat32Error::NotFound)?;
        if entry.is_dir {
            return Err(Fat32Error::NotAFile);
        }
        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster)?;
        }
        let mut raw = self.read_cluster(cluster)?;
        raw[offset] = 0xE5;
        self.write_cluster(cluster, &raw)
    }
    /// Create an empty directory at `path`. The parent must exist.
    pub fn create_dir(&mut self, path: &str) -> Result<(), Fat32Error> {
        let (parent_path, name) = split_path(path);
        if name.is_empty() {
            return Err(Fat32Error::AlreadyExists); // the root
        }
        let short_name = to_short_name(name)?;
        let parent = self.resolve_dir(parent_path)?;
        if self.locate_entry(parent, &short_name)?.is_some() {
            return Err(Fat32Error::AlreadyExists);
        }

        // A directory's first cluster starts with "." (itself) and ".."
        // (its parent; 0 stands for the root) — then zeroes, which read
        // as "end of directory".
        let cluster = self.alloc_clusters(1)?[0];
        let mut buf = vec![0u8; self.layout.cluster_size];
        let root = self.root_cluster();
        let parent_ref = if parent == root { 0 } else { parent };
        Self::write_dot_entry(&mut buf[0..32], b".          ", cluster);
        Self::write_dot_entry(&mut buf[32..64], b"..         ", parent_ref);
        self.write_cluster(cluster, &buf)?;

        let (slot_cluster, slot_offset) = self.find_free_slot(parent)?;
        self.write_entry_slot(slot_cluster, slot_offset, &short_name, true, 0, cluster)
    }

    fn write_dot_entry(slot: &mut [u8], name: &[u8; 11], cluster: u32) {
        slot.fill(0);
        slot[0..11].copy_from_slice(name);
        slot[11] = ATTR_DIRECTORY;
        slot[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        slot[26..28].copy_from_slice(&((cluster & 0xFFFF) as u16).to_le_bytes());
    }

    /// Remove the (empty) directory at `path`.
    pub fn remove_dir(&mut self, path: &str) -> Result<(), Fat32Error> {
        let (parent_path, name) = split_path(path);
        if name.is_empty() {
            return Err(Fat32Error::Invalid); // can't remove the root
        }
        let short_name = to_short_name(name)?;
        let parent = self.resolve_dir(parent_path)?;
        let (slot_cluster, offset, entry) = self
            .locate_entry(parent, &short_name)?
            .ok_or(Fat32Error::NotFound)?;
        if !entry.is_dir {
            return Err(Fat32Error::NotADirectory);
        }
        if !self.read_dir(entry.first_cluster)?.is_empty() {
            return Err(Fat32Error::NotEmpty);
        }
        self.free_chain(entry.first_cluster)?;
        let mut raw = self.read_cluster(slot_cluster)?;
        raw[offset] = 0xE5;
        self.write_cluster(slot_cluster, &raw)
    }

    /// Move/rename `from` to `to` (files or directories, across
    /// directories). An existing *file* at `to` is replaced when `from`
    /// is a file too, like `rename(2)`; anything else at `to` is
    /// `AlreadyExists`.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), Fat32Error> {
        let (from_dir_path, from_name) = split_path(from);
        let (to_dir_path, to_name) = split_path(to);
        if from_name.is_empty() || to_name.is_empty() {
            return Err(Fat32Error::Invalid);
        }
        let from_short = to_short_name(from_name)?;
        let to_short = to_short_name(to_name)?;
        let from_dir = self.resolve_dir(from_dir_path)?;
        let (old_cluster, old_offset, entry) = self
            .locate_entry(from_dir, &from_short)?
            .ok_or(Fat32Error::NotFound)?;

        let to_dir = self.resolve_dir(to_dir_path)?;
        if entry.is_dir {
            // Not into itself or its own subtree.
            let norm = |p: &str| p.trim_matches('/').to_ascii_uppercase();
            let (f, t) = (norm(from), norm(to));
            if t.starts_with(&(f + "/")) {
                return Err(Fat32Error::Invalid);
            }
        }
        if from_dir == to_dir && from_short == to_short {
            return Ok(()); // renaming onto itself
        }

        if let Some((c, o, existing)) = self.locate_entry(to_dir, &to_short)? {
            if existing.is_dir || entry.is_dir {
                return Err(Fat32Error::AlreadyExists);
            }
            // Replace the old file: free its data and reuse its slot.
            if existing.first_cluster >= 2 {
                self.free_chain(existing.first_cluster)?;
            }
            let mut raw = self.read_cluster(c)?;
            raw[o] = 0xE5;
            self.write_cluster(c, &raw)?;
        }

        // Link under the new name first, then unlink the old slot, so a
        // failure in between leaves a duplicate rather than a lost file.
        let (new_cluster, new_offset) = self.find_free_slot(to_dir)?;
        self.write_entry_slot(
            new_cluster,
            new_offset,
            &to_short,
            entry.is_dir,
            entry.size as u32,
            entry.first_cluster,
        )?;
        // find_free_slot may hand back the very slot we're vacating only
        // if it was already deleted, which it isn't yet.
        let mut raw = self.read_cluster(old_cluster)?;
        raw[old_offset] = 0xE5;
        self.write_cluster(old_cluster, &raw)?;

        // A moved directory's ".." must point at its new parent.
        if entry.is_dir && from_dir != to_dir {
            let root = self.root_cluster();
            let parent_ref = if to_dir == root { 0 } else { to_dir };
            let mut dir = self.read_cluster(entry.first_cluster)?;
            dir[32 + 20..32 + 22].copy_from_slice(&((parent_ref >> 16) as u16).to_le_bytes());
            dir[32 + 26..32 + 28].copy_from_slice(&((parent_ref & 0xFFFF) as u16).to_le_bytes());
            self.write_cluster(entry.first_cluster, &dir)?;
        }
        Ok(())
    }
}

impl FileSystem for Fat32Fs {
    fn fs_type(&self) -> &'static str {
        "fat32"
    }

    fn read_dir_at(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        Fat32Fs::read_dir_at(self, path)
    }

    fn find(&mut self, path: &str) -> FsResult<DirEntry> {
        Fat32Fs::find(self, path)
    }

    fn read_file(&mut self, entry: &DirEntry) -> FsResult<Vec<u8>> {
        Fat32Fs::read_file(self, entry)
    }

    fn write_file(&mut self, path: &str, data: &[u8]) -> FsResult<()> {
        Fat32Fs::write_file(self, path, data)
    }

    fn delete_file(&mut self, path: &str) -> FsResult<()> {
        Fat32Fs::delete_file(self, path)
    }

    fn create_dir(&mut self, path: &str) -> FsResult<()> {
        Fat32Fs::create_dir(self, path)
    }

    fn remove_dir(&mut self, path: &str) -> FsResult<()> {
        Fat32Fs::remove_dir(self, path)
    }

    fn rename(&mut self, from: &str, to: &str) -> FsResult<()> {
        Fat32Fs::rename(self, from, to)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::ramdisk::RamDisk;

    fn fs() -> Fat32Fs {
        Fat32Fs::mount(Box::new(RamDisk::new())).unwrap()
    }

    fn names(fs: &mut Fat32Fs, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = fs.read_dir_at(dir).unwrap().into_iter().map(|e| e.name).collect();
        v.sort();
        v
    }

    #[test]
    fn mkdir_then_files_inside() {
        let mut fs = fs();
        fs.create_dir("/docs").unwrap();
        fs.create_dir("/docs/sub").unwrap();
        fs.write_file("/docs/sub/a.txt", b"hello").unwrap();
        assert_eq!(names(&mut fs, "/"), ["DOCS"]);
        assert_eq!(names(&mut fs, "/docs"), ["SUB"]);
        assert_eq!(fs.read_file_at("/docs/sub/a.txt").unwrap(), b"hello");
        assert!(fs.find("/docs").unwrap().is_dir);
    }

    #[test]
    fn mkdir_existing_fails_and_missing_parent_fails() {
        let mut fs = fs();
        fs.create_dir("/a").unwrap();
        assert_eq!(fs.create_dir("/a"), Err(Fat32Error::AlreadyExists));
        assert_eq!(fs.create_dir("/nope/b"), Err(Fat32Error::NotFound));
        fs.write_file("/f.txt", b"x").unwrap();
        assert_eq!(fs.create_dir("/f.txt"), Err(Fat32Error::AlreadyExists));
    }

    #[test]
    fn dot_entries_are_hidden_but_present_on_disk() {
        let mut fs = fs();
        fs.create_dir("/d").unwrap();
        fs.create_dir("/d/e").unwrap();
        let d = fs.find("/d").unwrap();
        let raw = fs.read_cluster(d.first_cluster).unwrap();
        assert_eq!(&raw[0..2], b". ");
        assert_eq!(&raw[32..35], b".. ");
        // ".." of a child of the root is cluster 0; of /d/e it's /d's.
        assert_eq!(le_u16(&raw, 32 + 26), 0);
        let e = fs.find("/d/e").unwrap();
        let eraw = fs.read_cluster(e.first_cluster).unwrap();
        assert_eq!(le_u16(&eraw, 32 + 26) as u32, d.first_cluster);
    }

    #[test]
    fn rmdir_rules() {
        let mut fs = fs();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f.txt", b"x").unwrap();
        assert_eq!(fs.remove_dir("/d"), Err(Fat32Error::NotEmpty));
        assert_eq!(fs.remove_dir("/d/f.txt"), Err(Fat32Error::NotADirectory));
        assert_eq!(fs.remove_dir("/"), Err(Fat32Error::Invalid));
        assert_eq!(fs.remove_dir("/zzz"), Err(Fat32Error::NotFound));
        fs.delete_file("/d/f.txt").unwrap();
        fs.remove_dir("/d").unwrap();
        assert!(names(&mut fs, "/").is_empty());
    }

    #[test]
    fn rmdir_frees_its_cluster() {
        let mut fs = fs();
        // The ramdisk is tiny; if rmdir leaked, this would run out.
        for _ in 0..600 {
            fs.create_dir("/tmp").unwrap();
            fs.remove_dir("/tmp").unwrap();
        }
    }

    #[test]
    fn rename_file_same_dir_and_across_dirs() {
        let mut fs = fs();
        fs.create_dir("/d").unwrap();
        fs.write_file("/a.txt", b"data").unwrap();
        fs.rename("/a.txt", "/b.txt").unwrap();
        assert_eq!(fs.find("/a.txt").unwrap_err(), Fat32Error::NotFound);
        fs.rename("/b.txt", "/d/c.txt").unwrap();
        assert_eq!(fs.read_file_at("/d/c.txt").unwrap(), b"data");
        assert_eq!(names(&mut fs, "/"), ["D"]);
    }

    #[test]
    fn rename_replaces_file_but_not_dir() {
        let mut fs = fs();
        fs.write_file("/a.txt", b"new").unwrap();
        fs.write_file("/b.txt", b"old").unwrap();
        fs.rename("/a.txt", "/b.txt").unwrap();
        assert_eq!(fs.read_file_at("/b.txt").unwrap(), b"new");
        assert_eq!(names(&mut fs, "/"), ["B.TXT"]);

        fs.create_dir("/d").unwrap();
        assert_eq!(fs.rename("/b.txt", "/d"), Err(Fat32Error::AlreadyExists));
        assert_eq!(fs.rename("/d", "/b.txt"), Err(Fat32Error::AlreadyExists));
        assert_eq!(fs.rename("/nope", "/x"), Err(Fat32Error::NotFound));
    }

    #[test]
    fn rename_dir_updates_dotdot_and_rejects_cycles() {
        let mut fs = fs();
        fs.create_dir("/a").unwrap();
        fs.create_dir("/b").unwrap();
        fs.write_file("/a/f.txt", b"x").unwrap();
        fs.rename("/a", "/b/a2").unwrap();
        assert_eq!(fs.read_file_at("/b/a2/f.txt").unwrap(), b"x");
        let b = fs.find("/b").unwrap();
        let a2 = fs.find("/b/a2").unwrap();
        let raw = fs.read_cluster(a2.first_cluster).unwrap();
        assert_eq!(le_u16(&raw, 32 + 26) as u32, b.first_cluster);
        assert_eq!(fs.rename("/b", "/b/a2/inside"), Err(Fat32Error::Invalid));
        fs.rename("/b", "/b").unwrap(); // onto itself: no-op
    }

    #[test]
    fn rename_onto_itself_is_a_noop() {
        let mut fs = fs();
        fs.write_file("/a.txt", b"x").unwrap();
        fs.rename("/a.txt", "/A.TXT").unwrap();
        assert_eq!(fs.read_file_at("/a.txt").unwrap(), b"x");
    }
}
