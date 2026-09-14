//! Block device abstraction shared by every storage backend, so
//! `fat32` doesn't need to know whether it's talking to a real ATA disk
//! or an in-memory initrd image.

use crate::drivers::ata;

/// Every block device here speaks in fixed 512-byte blocks — matches
/// both the ATA sector size and the FAT32 volumes we expect to mount
/// (`Fat32Fs::mount` rejects anything with a different BPB sector size).
pub const BLOCK_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// Block index is past the end of the device.
    OutOfRange,
    /// The underlying hardware reported an error.
    Io,
    /// Device doesn't support writes (e.g. the initrd).
    ReadOnly,
}

/// A random-access block device: something `fat32::Fat32Fs` can mount.
pub trait BlockDevice: Send {
    /// Total number of `BLOCK_SIZE`-byte blocks on this device.
    fn block_count(&self) -> u64;

    /// Read one block into `buf`, which must be at least `BLOCK_SIZE` bytes.
    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write one block from `buf`, which must be at least `BLOCK_SIZE` bytes.
    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), BlockError>;
}

/// The primary ATA bus's master drive, addressed one 512-byte sector
/// (= one block) at a time.
pub struct AtaDisk {
    sector_count: u64,
}

impl AtaDisk {
    /// Probe the primary master drive with IDENTIFY. Returns `None` if
    /// nothing answers.
    pub fn probe() -> Option<Self> {
        let info = ata::identify().ok()?;
        Some(Self {
            sector_count: info.sector_count as u64,
        })
    }
}

impl BlockDevice for AtaDisk {
    fn block_count(&self) -> u64 {
        self.sector_count
    }

    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if index >= self.sector_count || buf.len() < BLOCK_SIZE {
            return Err(BlockError::OutOfRange);
        }
        ata::read_sectors(index as u32, 1, buf).map_err(|_| BlockError::Io)
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), BlockError> {
        if index >= self.sector_count || buf.len() < BLOCK_SIZE {
            return Err(BlockError::OutOfRange);
        }
        ata::write_sectors(index as u32, 1, buf).map_err(|_| BlockError::Io)
    }
}

/// A read-only disk image sitting in memory — backs the initrd, which
/// GRUB loads as a multiboot2 module before jumping into the kernel.
pub struct MemDisk {
    data: &'static [u8],
}

impl MemDisk {
    pub fn new(data: &'static [u8]) -> Self {
        Self { data }
    }
}

impl BlockDevice for MemDisk {
    fn block_count(&self) -> u64 {
        (self.data.len() / BLOCK_SIZE) as u64
    }

    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let offset = index as usize * BLOCK_SIZE;
        if offset + BLOCK_SIZE > self.data.len() || buf.len() < BLOCK_SIZE {
            return Err(BlockError::OutOfRange);
        }
        buf[..BLOCK_SIZE].copy_from_slice(&self.data[offset..offset + BLOCK_SIZE]);
        Ok(())
    }

    fn write_block(&mut self, _index: u64, _buf: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::ReadOnly)
    }
}
