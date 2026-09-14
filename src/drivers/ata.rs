//! ATA PIO driver — primary bus, master drive only.
//!
//! Polling PIO mode: no DMA, no interrupts, just busy-waiting on status
//! bits. Slower than DMA/AHCI, but it's a fraction of the code and plenty
//! fast for the small, mostly-sequential reads/writes a FAT32 driver
//! does. No secondary bus, no slave drive, no ATAPI, no AHCI/SATA — see
//! `todo.md` if any of that ever becomes worth the complexity.
//!
//! Reference: <https://wiki.osdev.org/ATA_PIO_Mode>

use crate::arch::port::{inb, inw, outb, outw};

/// Bytes per sector. Every command here assumes this (LBA28 addressing
/// counts in sectors, not bytes).
pub const SECTOR_SIZE: usize = 512;

// --- primary bus I/O ports ------------------------------------------------

const DATA: u16 = 0x1F0; // 16-bit data register
const ERROR: u16 = 0x1F1; // read: error, write: features (unused, always 0)
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LOW: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HIGH: u16 = 0x1F5;
const DRIVE_HEAD: u16 = 0x1F6;
const STATUS: u16 = 0x1F7; // read
const COMMAND: u16 = 0x1F7; // write
const CONTROL: u16 = 0x3F6; // device control / alternate status (no IRQ ack side effect)

// --- status register bits -------------------------------------------------

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_DF: u8 = 1 << 5;
const STATUS_BSY: u8 = 1 << 7;

// --- commands --------------------------------------------------------------

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_FLUSH_CACHE: u8 = 0xE7;
const CMD_IDENTIFY: u8 = 0xEC;

/// How many times to spin waiting on a status bit before giving up. This
/// is a polling driver with no timer-based timeout, so it's just a large
/// bounded loop rather than a real deadline.
const POLL_ATTEMPTS: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtaError {
    /// Nothing answered on the bus (floating/absent drive).
    NoDrive,
    /// Spun past `POLL_ATTEMPTS` waiting on BSY/DRQ.
    Timeout,
    /// Drive set ERR or DF in the status register.
    DeviceFault,
}

#[derive(Debug, Clone, Copy)]
pub struct DriveInfo {
    /// Total addressable sectors (LBA28, from IDENTIFY words 60-61).
    pub sector_count: u32,
}

impl DriveInfo {
    pub fn capacity_bytes(&self) -> u64 {
        self.sector_count as u64 * SECTOR_SIZE as u64
    }
}

/// 400ns delay by reading the alternate status register four times (each
/// read takes ~100ns on real hardware) — the standard trick for letting
/// drive electronics settle after a drive/head select.
fn delay_400ns() {
    for _ in 0..4 {
        unsafe { inb(CONTROL) };
    }
}

/// Poll until BSY clears, returning the status register's final value.
fn wait_not_busy() -> Result<u8, AtaError> {
    for _ in 0..POLL_ATTEMPTS {
        let status = unsafe { inb(STATUS) };
        if status & STATUS_BSY == 0 {
            return Ok(status);
        }
        core::hint::spin_loop();
    }
    Err(AtaError::Timeout)
}

/// Poll until DRQ (data ready) sets, or bail out on ERR/DF.
fn wait_drq() -> Result<(), AtaError> {
    for _ in 0..POLL_ATTEMPTS {
        let status = unsafe { inb(STATUS) };
        if status & (STATUS_ERR | STATUS_DF) != 0 {
            return Err(AtaError::DeviceFault);
        }
        if status & STATUS_DRQ != 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(AtaError::Timeout)
}

/// Select the master drive and load the LBA28 address + sector count for
/// a read/write command, but don't issue the command itself.
fn setup_lba28(lba: u32, sector_count: u8) -> Result<(), AtaError> {
    unsafe {
        // 0xE0 = LBA mode, master drive; top 4 LBA bits go in the low nibble.
        outb(DRIVE_HEAD, 0xE0 | (((lba >> 24) & 0x0F) as u8));
    }
    delay_400ns();
    wait_not_busy()?;
    unsafe {
        outb(ERROR, 0); // features: unused
        outb(SECTOR_COUNT, sector_count);
        outb(LBA_LOW, (lba & 0xFF) as u8);
        outb(LBA_MID, ((lba >> 8) & 0xFF) as u8);
        outb(LBA_HIGH, ((lba >> 16) & 0xFF) as u8);
    }
    Ok(())
}

/// Probe the primary master drive with IDENTIFY DEVICE.
///
/// Returns `Err(AtaError::NoDrive)` if nothing is attached, or if
/// whatever answered isn't a plain ATA drive (e.g. ATAPI).
pub fn identify() -> Result<DriveInfo, AtaError> {
    unsafe {
        outb(DRIVE_HEAD, 0xE0);
    }
    delay_400ns();
    unsafe {
        outb(SECTOR_COUNT, 0);
        outb(LBA_LOW, 0);
        outb(LBA_MID, 0);
        outb(LBA_HIGH, 0);
        outb(COMMAND, CMD_IDENTIFY);
    }

    let status = unsafe { inb(STATUS) };
    if status == 0 || status == 0xFF {
        return Err(AtaError::NoDrive); // floating bus: nothing attached
    }

    wait_not_busy()?;

    // A real ATA drive leaves these zeroed after IDENTIFY; ATAPI/SATA
    // devices signal their presence here instead. We don't speak either.
    let lba_mid = unsafe { inb(LBA_MID) };
    let lba_high = unsafe { inb(LBA_HIGH) };
    if lba_mid != 0 || lba_high != 0 {
        return Err(AtaError::NoDrive);
    }

    wait_drq()?;

    let mut raw = [0u16; 256];
    for word in raw.iter_mut() {
        *word = unsafe { inw(DATA) };
    }

    // LBA28 total addressable sectors: words 60 (low) and 61 (high).
    let sector_count = (raw[60] as u32) | ((raw[61] as u32) << 16);
    Ok(DriveInfo { sector_count })
}

/// Read `sector_count` consecutive sectors starting at `lba` into `buf`.
///
/// `buf` must be at least `sector_count as usize * SECTOR_SIZE` bytes.
pub fn read_sectors(lba: u32, sector_count: u8, buf: &mut [u8]) -> Result<(), AtaError> {
    assert!(
        buf.len() >= sector_count as usize * SECTOR_SIZE,
        "read_sectors: buffer too small"
    );
    setup_lba28(lba, sector_count)?;
    unsafe { outb(COMMAND, CMD_READ_SECTORS) };

    for sector in 0..sector_count as usize {
        wait_drq()?;
        let offset = sector * SECTOR_SIZE;
        for word in 0..(SECTOR_SIZE / 2) {
            let value = unsafe { inw(DATA) };
            buf[offset + word * 2] = (value & 0xFF) as u8;
            buf[offset + word * 2 + 1] = (value >> 8) as u8;
        }
    }
    Ok(())
}

/// Write `sector_count` consecutive sectors starting at `lba` from `buf`,
/// then flush the drive's write cache so the data is actually durable.
///
/// `buf` must be at least `sector_count as usize * SECTOR_SIZE` bytes.
pub fn write_sectors(lba: u32, sector_count: u8, buf: &[u8]) -> Result<(), AtaError> {
    assert!(
        buf.len() >= sector_count as usize * SECTOR_SIZE,
        "write_sectors: buffer too small"
    );
    setup_lba28(lba, sector_count)?;
    unsafe { outb(COMMAND, CMD_WRITE_SECTORS) };

    for sector in 0..sector_count as usize {
        wait_drq()?;
        let offset = sector * SECTOR_SIZE;
        for word in 0..(SECTOR_SIZE / 2) {
            let value = buf[offset + word * 2] as u16 | ((buf[offset + word * 2 + 1] as u16) << 8);
            unsafe { outw(DATA, value) };
        }
    }

    unsafe { outb(COMMAND, CMD_FLUSH_CACHE) };
    wait_not_busy()?;
    Ok(())
}
