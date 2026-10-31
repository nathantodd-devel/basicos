//! Partition tables: find the volumes on a disk.
//!
//! Handles the three layouts a disk shows up in:
//!
//! * **no table at all** ("superfloppy"): the filesystem starts at LBA 0
//!   — how the kernel's test disk image is built;
//! * **MBR**, including logical partitions inside an extended partition;
//! * **GPT** (found through the protective MBR entry, type `0xEE`).
//!
//! [`scan`] only reports *where* things are; [`Partition`] turns one into
//! a [`BlockDevice`] over a shared disk so a filesystem can mount it
//! without knowing it isn't the whole device.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::block::{BlockDevice, BlockError, BLOCK_SIZE};

/// One volume-sized region of a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartInfo {
    /// First sector (LBA).
    pub start: u64,
    /// Length in sectors.
    pub sectors: u64,
    /// MBR type byte (0 for GPT and whole-disk).
    pub mbr_type: u8,
    /// 1-based position for display; 0 = the whole disk.
    pub number: u8,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Does sector 0 look like a FAT boot sector rather than an MBR? (Both end
/// in 0x55AA and both may start with a jump; the BPB is what tells them
/// apart.)
fn looks_like_fat_bpb(s0: &[u8]) -> bool {
    let bytes_per_sector = le16(s0, 11);
    let sectors_per_cluster = s0[13];
    (s0[0] == 0xEB || s0[0] == 0xE9)
        && bytes_per_sector == 512
        && sectors_per_cluster.is_power_of_two()
        && (&s0[0x52..0x57] == b"FAT32" || &s0[0x36..0x3B] == b"FAT12" || &s0[0x36..0x3B] == b"FAT16")
}

/// Does sector 0 look like a superblock-less start of a filesystem we
/// should treat as a whole-disk volume? ext2/3/4 keeps its superblock at
/// byte 1024, so a partitionless ext4 disk has *no* signature in LBA 0
/// (usually all zeros) — check for the ext magic in LBA 2.
fn looks_like_ext(dev: &mut dyn BlockDevice) -> bool {
    let mut b = [0u8; BLOCK_SIZE];
    dev.read_block(2, &mut b).is_ok() && le16(&b, 0x38) == 0xEF53
}

/// Enumerate the volumes on `dev`.
pub fn scan(dev: &mut dyn BlockDevice) -> Vec<PartInfo> {
    let total = dev.block_count();
    let whole = || PartInfo { start: 0, sectors: total, mbr_type: 0, number: 0 };
    let mut s0 = [0u8; BLOCK_SIZE];
    if dev.read_block(0, &mut s0).is_err() {
        return Vec::new();
    }
    if looks_like_ext(dev) {
        return alloc::vec![whole()];
    }
    if s0[510] != 0x55 || s0[511] != 0xAA || looks_like_fat_bpb(&s0) {
        // No partition table: if it's a FAT volume, it's the whole disk.
        return if looks_like_fat_bpb(&s0) { alloc::vec![whole()] } else { Vec::new() };
    }

    let mut out = Vec::new();
    let mut number = 0u8;
    let mut extended_base: Option<u64> = None;
    for i in 0..4 {
        let e = &s0[446 + i * 16..446 + (i + 1) * 16];
        let (ty, start, len) = (e[4], le32(e, 8) as u64, le32(e, 12) as u64);
        if ty == 0 || len == 0 {
            continue;
        }
        match ty {
            0xEE => return scan_gpt(dev, total),
            0x05 | 0x0F | 0x85 => extended_base = extended_base.or(Some(start)),
            _ => {
                number += 1;
                if start + len <= total {
                    out.push(PartInfo { start, sectors: len, mbr_type: ty, number });
                }
            }
        }
    }

    // Logical partitions: a chain of EBRs. Each EBR's first entry is the
    // partition (start relative to that EBR), its second links to the next
    // EBR (relative to the extended partition's own start).
    if let Some(base) = extended_base {
        let mut ebr = base;
        let mut number = 4u8;
        for _ in 0..64 {
            let mut b = [0u8; BLOCK_SIZE];
            if dev.read_block(ebr, &mut b).is_err() || b[510] != 0x55 || b[511] != 0xAA {
                break;
            }
            let (ty, start, len) = (b[446 + 4], le32(&b, 446 + 8) as u64, le32(&b, 446 + 12) as u64);
            if ty != 0 && len != 0 && ebr + start + len <= total {
                number += 1;
                out.push(PartInfo { start: ebr + start, sectors: len, mbr_type: ty, number });
            }
            let (nty, nstart) = (b[462 + 4], le32(&b, 462 + 8) as u64);
            if !matches!(nty, 0x05 | 0x0F | 0x85) || nstart == 0 {
                break;
            }
            ebr = base + nstart;
        }
    }
    out
}

fn scan_gpt(dev: &mut dyn BlockDevice, total: u64) -> Vec<PartInfo> {
    let mut h = [0u8; BLOCK_SIZE];
    if dev.read_block(1, &mut h).is_err() || &h[0..8] != b"EFI PART" {
        return Vec::new();
    }
    let entries_lba = le64(&h, 72);
    let count = le32(&h, 80) as u64;
    let entry_size = le32(&h, 84) as u64;
    if entry_size < 128 || entry_size > 4096 || count > 1024 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut number = 0u8;
    let per_sector = BLOCK_SIZE as u64 / entry_size.min(BLOCK_SIZE as u64);
    let _ = per_sector;
    for i in 0..count {
        let byte = i * entry_size;
        let lba = entries_lba + byte / BLOCK_SIZE as u64;
        let off = (byte % BLOCK_SIZE as u64) as usize;
        let mut b = [0u8; BLOCK_SIZE];
        if dev.read_block(lba, &mut b).is_err() || off + 48 > BLOCK_SIZE {
            break;
        }
        let e = &b[off..];
        if e[0..16].iter().all(|&x| x == 0) {
            continue; // unused slot
        }
        let (first, last) = (le64(e, 32), le64(e, 40));
        number = number.saturating_add(1);
        if last >= first && last < total {
            out.push(PartInfo { start: first, sectors: last - first + 1, mbr_type: 0, number });
        }
    }
    out
}

/// A disk that several [`Partition`]s share. The kernel implements this
/// over its ATA driver with a lock; tests over an in-memory image.
pub trait SharedDisk: Send + Sync {
    fn block_count(&self) -> u64;
    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_block(&self, index: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// Multi-block transfers; devices with a fast path override these.
    fn read_blocks(&self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        for (i, c) in buf.chunks_mut(BLOCK_SIZE).enumerate() {
            self.read_block(start + i as u64, c)?;
        }
        Ok(())
    }
    fn write_blocks(&self, start: u64, buf: &[u8]) -> Result<(), BlockError> {
        for (i, c) in buf.chunks(BLOCK_SIZE).enumerate() {
            self.write_block(start + i as u64, c)?;
        }
        Ok(())
    }
}

/// A window `[start, start+sectors)` onto a [`SharedDisk`], as a
/// [`BlockDevice`] whose block 0 is the window's first sector.
pub struct Partition {
    disk: Arc<dyn SharedDisk>,
    start: u64,
    sectors: u64,
    read_only: bool,
}

impl Partition {
    pub fn new(disk: Arc<dyn SharedDisk>, info: PartInfo) -> Partition {
        Partition { disk, start: info.start, sectors: info.sectors, read_only: false }
    }

    /// Refuse writes (for volumes the driver can't safely update).
    pub fn read_only(mut self) -> Partition {
        self.read_only = true;
        self
    }
}

impl BlockDevice for Partition {
    fn block_count(&self) -> u64 {
        self.sectors
    }

    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if index >= self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.disk.read_block(self.start + index, buf)
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        if index >= self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.disk.write_block(self.start + index, buf)
    }

    fn read_blocks(&mut self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let n = (buf.len() / BLOCK_SIZE) as u64;
        if start + n > self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.disk.read_blocks(self.start + start, buf)
    }

    fn write_blocks(&mut self, start: u64, buf: &[u8]) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        let n = (buf.len() / BLOCK_SIZE) as u64;
        if start + n > self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.disk.write_blocks(self.start + start, buf)
    }
}

/// A `Vec`-backed [`SharedDisk`] behind a lock-free-enough interior — for
/// tests and host tools, not the kernel.
#[cfg(test)]
pub mod mem {
    extern crate std;
    use super::*;
    use std::sync::Mutex;

    pub struct MemShared(pub Mutex<Vec<u8>>);

    impl SharedDisk for MemShared {
        fn block_count(&self) -> u64 {
            (self.0.lock().unwrap().len() / BLOCK_SIZE) as u64
        }
        fn read_block(&self, i: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let d = self.0.lock().unwrap();
            let o = i as usize * BLOCK_SIZE;
            if o + BLOCK_SIZE > d.len() {
                return Err(BlockError::OutOfRange);
            }
            buf[..BLOCK_SIZE].copy_from_slice(&d[o..o + BLOCK_SIZE]);
            Ok(())
        }
        fn write_block(&self, i: u64, buf: &[u8]) -> Result<(), BlockError> {
            let mut d = self.0.lock().unwrap();
            let o = i as usize * BLOCK_SIZE;
            if o + BLOCK_SIZE > d.len() {
                return Err(BlockError::OutOfRange);
            }
            d[o..o + BLOCK_SIZE].copy_from_slice(&buf[..BLOCK_SIZE]);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::mem::MemShared;
    use super::*;
    use std::sync::Mutex;

    fn disk(sectors: usize) -> Vec<u8> {
        std::vec![0u8; sectors * BLOCK_SIZE]
    }

    struct Dev(Vec<u8>);
    impl BlockDevice for Dev {
        fn block_count(&self) -> u64 {
            (self.0.len() / BLOCK_SIZE) as u64
        }
        fn read_block(&mut self, i: u64, b: &mut [u8]) -> Result<(), BlockError> {
            let o = i as usize * BLOCK_SIZE;
            b[..BLOCK_SIZE].copy_from_slice(self.0.get(o..o + BLOCK_SIZE).ok_or(BlockError::OutOfRange)?);
            Ok(())
        }
        fn write_block(&mut self, i: u64, b: &[u8]) -> Result<(), BlockError> {
            let o = i as usize * BLOCK_SIZE;
            self.0[o..o + BLOCK_SIZE].copy_from_slice(&b[..BLOCK_SIZE]);
            Ok(())
        }
    }

    fn mbr_entry(d: &mut [u8], slot: usize, ty: u8, start: u32, len: u32) {
        let e = &mut d[446 + slot * 16..446 + (slot + 1) * 16];
        e[4] = ty;
        e[8..12].copy_from_slice(&start.to_le_bytes());
        e[12..16].copy_from_slice(&len.to_le_bytes());
    }

    #[test]
    fn empty_disk_has_no_volumes() {
        assert!(scan(&mut Dev(disk(100))).is_empty());
    }

    #[test]
    fn fat_without_table_is_whole_disk() {
        let mut d = disk(100);
        d[0] = 0xEB;
        d[11..13].copy_from_slice(&512u16.to_le_bytes());
        d[13] = 8;
        d[0x52..0x57].copy_from_slice(b"FAT32");
        d[510] = 0x55;
        d[511] = 0xAA;
        let p = scan(&mut Dev(d));
        assert_eq!(p, [PartInfo { start: 0, sectors: 100, mbr_type: 0, number: 0 }]);
    }

    #[test]
    fn ext_without_table_is_whole_disk() {
        let mut d = disk(100);
        d[2 * 512 + 0x38..2 * 512 + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        assert_eq!(scan(&mut Dev(d)).len(), 1);
    }

    #[test]
    fn mbr_primaries() {
        let mut d = disk(1000);
        d[510] = 0x55;
        d[511] = 0xAA;
        mbr_entry(&mut d, 0, 0x0C, 63, 100);
        mbr_entry(&mut d, 2, 0x83, 200, 300);
        let p = scan(&mut Dev(d));
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].start, p[0].sectors, p[0].mbr_type, p[0].number), (63, 100, 0x0C, 1));
        assert_eq!((p[1].start, p[1].sectors, p[1].mbr_type, p[1].number), (200, 300, 0x83, 2));
    }

    #[test]
    fn partition_past_end_is_ignored() {
        let mut d = disk(100);
        d[510] = 0x55;
        d[511] = 0xAA;
        mbr_entry(&mut d, 0, 0x83, 50, 500);
        assert!(scan(&mut Dev(d)).is_empty());
    }

    #[test]
    fn mbr_logical_partitions() {
        let mut d = disk(2000);
        d[510] = 0x55;
        d[511] = 0xAA;
        mbr_entry(&mut d, 0, 0x83, 10, 20);
        mbr_entry(&mut d, 1, 0x05, 100, 1000);
        // EBR #1 at 100: logical at +8 (len 50), link to EBR at 100+200.
        let e1 = 100 * 512;
        d[e1 + 510] = 0x55;
        d[e1 + 511] = 0xAA;
        d[e1 + 446 + 4] = 0x83;
        d[e1 + 446 + 8..e1 + 446 + 12].copy_from_slice(&8u32.to_le_bytes());
        d[e1 + 446 + 12..e1 + 446 + 16].copy_from_slice(&50u32.to_le_bytes());
        d[e1 + 462 + 4] = 0x05;
        d[e1 + 462 + 8..e1 + 462 + 12].copy_from_slice(&200u32.to_le_bytes());
        let e2 = 300 * 512;
        d[e2 + 510] = 0x55;
        d[e2 + 511] = 0xAA;
        d[e2 + 446 + 4] = 0x83;
        d[e2 + 446 + 8..e2 + 446 + 12].copy_from_slice(&8u32.to_le_bytes());
        d[e2 + 446 + 12..e2 + 446 + 16].copy_from_slice(&70u32.to_le_bytes());
        let p = scan(&mut Dev(d));
        let spans: Vec<(u64, u64, u8)> = p.iter().map(|x| (x.start, x.sectors, x.number)).collect();
        assert_eq!(spans, [(10, 20, 1), (108, 50, 5), (308, 70, 6)]);
    }

    #[test]
    fn gpt_partitions() {
        let mut d = disk(1000);
        d[510] = 0x55;
        d[511] = 0xAA;
        mbr_entry(&mut d, 0, 0xEE, 1, 999);
        let h = 512;
        d[h..h + 8].copy_from_slice(b"EFI PART");
        d[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes()); // entries at LBA 2
        d[h + 80..h + 84].copy_from_slice(&4u32.to_le_bytes());
        d[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
        for (slot, (first, last)) in [(34u64, 99u64), (100, 199)].iter().enumerate() {
            let e = 2 * 512 + slot * 128;
            d[e] = 0xAF; // any nonzero type GUID
            d[e + 32..e + 40].copy_from_slice(&first.to_le_bytes());
            d[e + 40..e + 48].copy_from_slice(&last.to_le_bytes());
        }
        let p = scan(&mut Dev(d));
        let spans: Vec<(u64, u64)> = p.iter().map(|x| (x.start, x.sectors)).collect();
        assert_eq!(spans, [(34, 66), (100, 100)]);
    }

    #[test]
    fn partition_window_maps_and_bounds() {
        let mut raw = disk(100);
        raw[50 * 512] = 0xAB;
        let shared: Arc<dyn SharedDisk> = Arc::new(MemShared(Mutex::new(raw)));
        let mut p = Partition::new(shared.clone(), PartInfo { start: 50, sectors: 10, mbr_type: 0, number: 1 });
        let mut b = [0u8; 512];
        p.read_block(0, &mut b).unwrap();
        assert_eq!(b[0], 0xAB);
        assert_eq!(p.read_block(10, &mut b), Err(BlockError::OutOfRange));
        b[0] = 0xCD;
        p.write_block(9, &b).unwrap();
        let mut c = [0u8; 512];
        shared.read_block(59, &mut c).unwrap();
        assert_eq!(c[0], 0xCD);
        let mut ro = Partition::new(shared, PartInfo { start: 0, sectors: 5, mbr_type: 0, number: 0 }).read_only();
        assert_eq!(ro.write_block(0, &b), Err(BlockError::ReadOnly));
    }
}
