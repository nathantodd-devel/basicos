//! A writable, in-memory FAT32 volume — the `A:` drive.
//!
//! Backed by a plain heap-allocated buffer, so `write_block` just writes
//! into it directly — unlike [`super::block::MemDisk`] (the initrd),
//! which wraps a `&'static [u8]` borrowed from a GRUB module it doesn't
//! own and can't grow or safely mutate.
//!
//! There's no way to get a real FAT32 image into RAM the way the initrd
//! gets one handed to it by GRUB, so [`RamDisk::new`] builds one from
//! scratch: a hand-written, minimal — but spec-correct for what
//! `fat32::Bpb::parse` actually checks — "mkfs.vfat" for one fixed,
//! small geometry. If the two ever drift apart, the tell is
//! `fs::init_ramdisk` failing to mount its own freshly-formatted disk.

use super::block::{BlockDevice, BlockError, BLOCK_SIZE};
use alloc::vec;
use alloc::vec::Vec;

/// Total ramdisk size: 256 KiB at 512 bytes/sector, 1 sector/cluster —
/// small enough to comfortably fit in one shot inside `mem::heap`
/// alongside everything else the heap does (see `HEAP_SIZE`'s own
/// comment), big enough to hold plenty of small files for a shell to
/// poke at. (Well under Microsoft's ~65 525-cluster minimum for a
/// "proper" FAT32 volume — same as the initrd image `make initrd`
/// builds, and just as harmless: nothing here enforces that minimum,
/// only real Windows does.)
pub const TOTAL_SECTORS: usize = 512; // 256 KiB / 512

// --- geometry, picked to divide out evenly (see `format_fat32`) ----------
const RESERVED_SECTORS: u32 = 32;
const NUM_FATS: u8 = 1;
const FAT_SIZE_SECTORS: u32 = 4; // 4 * 512 / 4 = 512 entries — covers every cluster below with room to spare
const ROOT_CLUSTER: u32 = 2;
const SECTORS_PER_CLUSTER: u8 = 1;

const FIRST_FAT_SECTOR: usize = RESERVED_SECTORS as usize;

/// Write a minimal FAT32 BPB + FAT + empty root directory into `buf`,
/// which must already be zeroed and exactly `TOTAL_SECTORS * BLOCK_SIZE`
/// bytes — zeroed is what makes the FAT's unused entries "free" and the
/// root directory "empty" (first byte 0x00) without touching them.
fn format_fat32(buf: &mut [u8]) {
    debug_assert_eq!(buf.len(), TOTAL_SECTORS * BLOCK_SIZE);

    // --- boot sector / BPB (sector 0) -------------------------------
    let b = &mut buf[0..BLOCK_SIZE];
    b[0] = 0xEB;
    b[1] = 0x00;
    b[2] = 0x90; // jmp short + nop; never executed, just conventional
    b[3..11].copy_from_slice(b"basicos "); // OEM name, 8 bytes
    b[11..13].copy_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());
    b[13] = SECTORS_PER_CLUSTER;
    b[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
    b[16] = NUM_FATS;
    // 17..19 root entry count: unused by FAT32 (0)
    // 19..21 total_sectors_16: left 0, forces the 32-bit field below
    b[21] = 0xF8; // media descriptor: fixed disk
    // 22..24 fat_size_16 MUST stay 0 — it's how Bpb::parse tells FAT32
    // apart from FAT12/16 (see its comment on the same check).
    // 24..32 sectors/track, heads, hidden sectors: unread by our driver
    b[32..36].copy_from_slice(&(TOTAL_SECTORS as u32).to_le_bytes());
    b[36..40].copy_from_slice(&FAT_SIZE_SECTORS.to_le_bytes());
    b[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    b[510] = 0x55;
    b[511] = 0xAA;

    // --- FAT (sectors RESERVED_SECTORS .. RESERVED_SECTORS+FAT_SIZE) ---
    let fat_start = FIRST_FAT_SECTOR * BLOCK_SIZE;
    let fat = &mut buf[fat_start..fat_start + FAT_SIZE_SECTORS as usize * BLOCK_SIZE];
    fat[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes()); // entry 0: media byte
    fat[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes()); // entry 1: reserved
    fat[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes()); // entry 2 (root): EOC

    // The root directory's one cluster, and every other FAT entry, are
    // already zero from the caller — zero is exactly "free" (FAT) and
    // "end of directory" (an empty dir's first byte), so there's
    // nothing left to write.
}

pub struct RamDisk {
    data: Vec<u8>,
}

impl RamDisk {
    /// Allocate and format a fresh, empty FAT32 ramdisk.
    pub fn new() -> Self {
        let mut data = vec![0u8; TOTAL_SECTORS * BLOCK_SIZE];
        format_fat32(&mut data);
        Self { data }
    }
}

impl BlockDevice for RamDisk {
    fn block_count(&self) -> u64 {
        TOTAL_SECTORS as u64
    }

    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let offset = index as usize * BLOCK_SIZE;
        if offset + BLOCK_SIZE > self.data.len() || buf.len() < BLOCK_SIZE {
            return Err(BlockError::OutOfRange);
        }
        buf[..BLOCK_SIZE].copy_from_slice(&self.data[offset..offset + BLOCK_SIZE]);
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), BlockError> {
        let offset = index as usize * BLOCK_SIZE;
        if offset + BLOCK_SIZE > self.data.len() || buf.len() < BLOCK_SIZE {
            return Err(BlockError::OutOfRange);
        }
        self.data[offset..offset + BLOCK_SIZE].copy_from_slice(&buf[..BLOCK_SIZE]);
        Ok(())
    }
}
