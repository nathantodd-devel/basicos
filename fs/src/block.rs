//! Block device abstraction shared by every storage backend, so
//! `fat32` doesn't need to know whether it's talking to a real ATA disk
//! or an in-memory initrd image.
//!
//! Only the hardware-agnostic pieces live here: the [`BlockDevice`]
//! trait itself and [`MemDisk`] (a read-only in-memory backend, useful
//! both for the kernel's initrd and for host-side tools testing against
//! a disk image file). A concrete backend that touches real hardware —
//! `AtaDisk`, in the kernel — implements [`BlockDevice`] but lives in
//! whichever binary actually has that hardware access; this crate has
//! none.

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

    /// Read `buf.len() / BLOCK_SIZE` consecutive blocks starting at
    /// `start`. Devices with a fast multi-block command override this;
    /// the default is one `read_block` each.
    fn read_blocks(&mut self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        for (i, chunk) in buf.chunks_mut(BLOCK_SIZE).enumerate() {
            self.read_block(start + i as u64, chunk)?;
        }
        Ok(())
    }

    /// Write `buf.len() / BLOCK_SIZE` consecutive blocks starting at `start`.
    fn write_blocks(&mut self, start: u64, buf: &[u8]) -> Result<(), BlockError> {
        for (i, chunk) in buf.chunks(BLOCK_SIZE).enumerate() {
            self.write_block(start + i as u64, chunk)?;
        }
        Ok(())
    }
}

/// A read-only disk image sitting in memory — backs the kernel's
/// initrd (a multiboot2 module GRUB loads before jumping into it), or
/// any other already-loaded byte slice a caller wants to mount.
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
