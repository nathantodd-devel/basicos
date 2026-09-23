//! The one [`BlockDevice`] implementation that needs real hardware
//! access, which is exactly why it lives here and not in the
//! `basicos-fs` crate (see that crate's own doc comment on the split).

use crate::drivers::ata;
use basicos_fs::{BlockDevice, BlockError, BLOCK_SIZE};

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
