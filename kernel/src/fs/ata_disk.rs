//! The [`BlockDevice`] implementations that need real hardware access,
//! which is exactly why they live here and not in the `basicos-fs` crate
//! (see that crate's own doc comment on the split).

use crate::drivers::ata::Drive;
use crate::sync::Spinlock;
use basicos_fs::partition::SharedDisk;
use basicos_fs::{BlockDevice, BlockError, BLOCK_SIZE};

/// One physical ATA drive, shared by however many partitions (each a
/// [`basicos_fs::partition::Partition`]) live on it. The lock serialises
/// commands; ATA is one command at a time per channel anyway.
pub struct AtaDisk {
    drive: Spinlock<Drive>,
    sectors: u64,
}

impl AtaDisk {
    pub fn new(drive: Drive) -> AtaDisk {
        let sectors = drive.sectors;
        AtaDisk { drive: Spinlock::new(drive), sectors }
    }

    pub fn describe(&self) -> (&'static str, alloc::string::String, u64) {
        let d = self.drive.lock();
        (d.position(), d.model.clone(), d.capacity_bytes())
    }
}

impl SharedDisk for AtaDisk {
    fn block_count(&self) -> u64 {
        self.sectors
    }

    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.read_blocks(index, &mut buf[..BLOCK_SIZE])
    }

    fn write_block(&self, index: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.write_blocks(index, &buf[..BLOCK_SIZE])
    }

    fn read_blocks(&self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let n = (buf.len() / BLOCK_SIZE) as u64;
        if start + n > self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.drive.lock().read(start, buf).map_err(|_| BlockError::Io)
    }

    fn write_blocks(&self, start: u64, buf: &[u8]) -> Result<(), BlockError> {
        let n = (buf.len() / BLOCK_SIZE) as u64;
        if start + n > self.sectors {
            return Err(BlockError::OutOfRange);
        }
        self.drive.lock().write(start, buf).map_err(|_| BlockError::Io)
    }
}

/// Adapter so a whole [`AtaDisk`] (no partition table) can be handed to a
/// filesystem as a plain [`BlockDevice`]. Partitions use
/// [`basicos_fs::partition::Partition`] over the same `Arc<AtaDisk>`.
pub struct WholeDisk(pub alloc::sync::Arc<AtaDisk>);

impl BlockDevice for WholeDisk {
    fn block_count(&self) -> u64 {
        self.0.sectors
    }
    fn read_block(&mut self, index: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.0.read_blocks(index, &mut buf[..BLOCK_SIZE])
    }
    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.0.write_blocks(index, &buf[..BLOCK_SIZE])
    }
    fn read_blocks(&mut self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.0.read_blocks(start, buf)
    }
    fn write_blocks(&mut self, start: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.0.write_blocks(start, buf)
    }
}

