//! ATA PIO driver — both legacy IDE channels, master and slave.
//!
//! Polling PIO mode: no DMA, no interrupts, just busy-waiting on status
//! bits. Slower than DMA/AHCI, but it's a fraction of the code and plenty
//! fast for what the filesystems here do. Up to four drives are found at
//! boot (primary/secondary channel × master/slave); LBA28 addressing is
//! used where it reaches, LBA48 beyond (or wherever the drive supports it
//! and the request needs it), so disks over 128 GiB work. No ATAPI (CD-ROMs
//! are recognised and skipped), no AHCI/SATA-native, no NVMe — QEMU's
//! `-drive if=ide` and real IDE/compat-mode SATA controllers are what this
//! talks to.
//!
//! Reference: <https://wiki.osdev.org/ATA_PIO_Mode>

use crate::arch::port::{inb, inw, outb, outw};

/// Bytes per sector. Every command here assumes this.
pub const SECTOR_SIZE: usize = 512;

/// One IDE channel's I/O ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    io: u16,
    ctrl: u16,
}

pub const PRIMARY: Channel = Channel { io: 0x1F0, ctrl: 0x3F6 };
pub const SECONDARY: Channel = Channel { io: 0x170, ctrl: 0x376 };

// Register offsets from `Channel::io`.
const DATA: u16 = 0;
const ERROR: u16 = 1; // read: error, write: features
const SECTOR_COUNT: u16 = 2;
const LBA_LOW: u16 = 3;
const LBA_MID: u16 = 4;
const LBA_HIGH: u16 = 5;
const DRIVE_HEAD: u16 = 6;
const STATUS: u16 = 7; // read
const COMMAND: u16 = 7; // write

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_DF: u8 = 1 << 5;
const STATUS_BSY: u8 = 1 << 7;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_READ_SECTORS_EXT: u8 = 0x24;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_WRITE_SECTORS_EXT: u8 = 0x34;
const CMD_FLUSH_CACHE: u8 = 0xE7;
const CMD_FLUSH_CACHE_EXT: u8 = 0xEA;
const CMD_IDENTIFY: u8 = 0xEC;

/// Spin count for waiting on a status bit: a bounded loop rather than a
/// real deadline, since this driver has no timer to lean on.
const POLL_ATTEMPTS: u32 = 5_000_000;

/// Sectors per command. (LBA28 allows 256, LBA48 65536; small keeps the
/// time spent with interrupts disabled — the callers hold spinlocks —
/// short.)
const MAX_SECTORS_PER_COMMAND: usize = 64;

/// LBA28 reaches this many sectors (128 GiB).
const LBA28_LIMIT: u64 = 1 << 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtaError {
    /// Nothing answered (floating/absent drive) or it isn't a plain ATA disk.
    NoDrive,
    /// Spun past `POLL_ATTEMPTS` waiting on BSY/DRQ.
    Timeout,
    /// Drive set ERR or DF in the status register.
    DeviceFault,
    /// LBA beyond the drive's capacity, or beyond what LBA28 can address
    /// on a drive without LBA48.
    OutOfRange,
}

/// One drive: which channel, master or slave, and what IDENTIFY said.
#[derive(Debug, Clone)]
pub struct Drive {
    chan: Channel,
    slave: bool,
    lba48: bool,
    /// Total addressable sectors.
    pub sectors: u64,
    /// Model string from IDENTIFY, trimmed.
    pub model: alloc::string::String,
}

impl Drive {
    pub fn capacity_bytes(&self) -> u64 {
        self.sectors * SECTOR_SIZE as u64
    }

    /// `"primary master"` etc., for logging.
    pub fn position(&self) -> &'static str {
        match (self.chan == PRIMARY, self.slave) {
            (true, false) => "primary master",
            (true, true) => "primary slave",
            (false, false) => "secondary master",
            (false, true) => "secondary slave",
        }
    }
}

/// 400ns delay by reading the alternate status register four times.
fn delay_400ns(c: Channel) {
    for _ in 0..4 {
        unsafe { inb(c.ctrl) };
    }
}

fn wait_not_busy(c: Channel) -> Result<u8, AtaError> {
    for _ in 0..POLL_ATTEMPTS {
        let status = unsafe { inb(c.io + STATUS) };
        if status & STATUS_BSY == 0 {
            return Ok(status);
        }
        core::hint::spin_loop();
    }
    Err(AtaError::Timeout)
}

fn wait_drq(c: Channel) -> Result<(), AtaError> {
    for _ in 0..POLL_ATTEMPTS {
        let status = unsafe { inb(c.io + STATUS) };
        if status & (STATUS_ERR | STATUS_DF) != 0 {
            return Err(AtaError::DeviceFault);
        }
        if status & STATUS_DRQ != 0 && status & STATUS_BSY == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(AtaError::Timeout)
}

fn select(c: Channel, slave: bool, lba_top: u8) {
    unsafe { outb(c.io + DRIVE_HEAD, 0xE0 | ((slave as u8) << 4) | (lba_top & 0x0F)) };
    delay_400ns(c);
}

/// Probe one position with IDENTIFY DEVICE.
pub fn identify(chan: Channel, slave: bool) -> Result<Drive, AtaError> {
    select(chan, slave, 0);
    unsafe {
        outb(chan.io + SECTOR_COUNT, 0);
        outb(chan.io + LBA_LOW, 0);
        outb(chan.io + LBA_MID, 0);
        outb(chan.io + LBA_HIGH, 0);
        outb(chan.io + COMMAND, CMD_IDENTIFY);
    }
    let status = unsafe { inb(chan.io + STATUS) };
    if status == 0 || status == 0xFF {
        return Err(AtaError::NoDrive); // nothing attached
    }
    wait_not_busy(chan)?;

    // ATAPI/SATA-native devices identify themselves here instead of
    // completing IDENTIFY; we don't speak either.
    let (mid, high) = unsafe { (inb(chan.io + LBA_MID), inb(chan.io + LBA_HIGH)) };
    if mid != 0 || high != 0 {
        return Err(AtaError::NoDrive);
    }
    wait_drq(chan)?;

    let mut raw = [0u16; 256];
    for word in raw.iter_mut() {
        *word = unsafe { inw(chan.io + DATA) };
    }

    let lba48 = raw[83] & (1 << 10) != 0;
    let sectors = if lba48 {
        (raw[100] as u64) | (raw[101] as u64) << 16 | (raw[102] as u64) << 32 | (raw[103] as u64) << 48
    } else {
        (raw[60] as u64) | (raw[61] as u64) << 16
    };
    if sectors == 0 {
        return Err(AtaError::NoDrive);
    }
    // Model: words 27..47, two ASCII bytes per word, high byte first.
    let mut model = alloc::string::String::new();
    for w in &raw[27..47] {
        model.push((w >> 8) as u8 as char);
        model.push((w & 0xFF) as u8 as char);
    }
    Ok(Drive { chan, slave, lba48, sectors, model: model.trim().into() })
}

/// Find every ATA disk on both channels.
pub fn probe_all() -> alloc::vec::Vec<Drive> {
    let mut found = alloc::vec::Vec::new();
    for chan in [PRIMARY, SECONDARY] {
        for slave in [false, true] {
            if let Ok(d) = identify(chan, slave) {
                found.push(d);
            }
        }
    }
    found
}

impl Drive {
    /// Load the address and count for a command and issue it.
    fn start(&self, lba: u64, count: usize, read: bool) -> Result<(), AtaError> {
        let c = self.chan;
        let ext = self.lba48 && (lba + count as u64 > LBA28_LIMIT || count > 255);
        if !self.lba48 && lba + count as u64 > LBA28_LIMIT {
            return Err(AtaError::OutOfRange);
        }
        unsafe {
            if ext {
                select(c, self.slave, 0);
                wait_not_busy(c)?;
                outb(c.io + ERROR, 0);
                outb(c.io + SECTOR_COUNT, (count >> 8) as u8);
                outb(c.io + LBA_LOW, (lba >> 24) as u8);
                outb(c.io + LBA_MID, (lba >> 32) as u8);
                outb(c.io + LBA_HIGH, (lba >> 40) as u8);
                outb(c.io + SECTOR_COUNT, count as u8);
                outb(c.io + LBA_LOW, lba as u8);
                outb(c.io + LBA_MID, (lba >> 8) as u8);
                outb(c.io + LBA_HIGH, (lba >> 16) as u8);
                outb(c.io + COMMAND, if read { CMD_READ_SECTORS_EXT } else { CMD_WRITE_SECTORS_EXT });
            } else {
                select(c, self.slave, (lba >> 24) as u8);
                wait_not_busy(c)?;
                outb(c.io + ERROR, 0);
                outb(c.io + SECTOR_COUNT, count as u8); // 0 would mean 256
                outb(c.io + LBA_LOW, lba as u8);
                outb(c.io + LBA_MID, (lba >> 8) as u8);
                outb(c.io + LBA_HIGH, (lba >> 16) as u8);
                outb(c.io + COMMAND, if read { CMD_READ_SECTORS } else { CMD_WRITE_SECTORS });
            }
        }
        Ok(())
    }

    /// Read `buf.len() / 512` sectors starting at `lba`.
    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), AtaError> {
        let total = buf.len() / SECTOR_SIZE;
        if lba + total as u64 > self.sectors {
            return Err(AtaError::OutOfRange);
        }
        let mut done = 0usize;
        while done < total {
            let n = (total - done).min(MAX_SECTORS_PER_COMMAND);
            self.start(lba + done as u64, n, true)?;
            for s in 0..n {
                wait_drq(self.chan)?;
                let base = (done + s) * SECTOR_SIZE;
                for w in 0..SECTOR_SIZE / 2 {
                    let v = unsafe { inw(self.chan.io + DATA) };
                    buf[base + w * 2] = v as u8;
                    buf[base + w * 2 + 1] = (v >> 8) as u8;
                }
            }
            done += n;
        }
        Ok(())
    }

    /// Write `buf.len() / 512` sectors starting at `lba`, then flush the
    /// drive's write cache so the data is durable.
    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<(), AtaError> {
        let total = buf.len() / SECTOR_SIZE;
        if lba + total as u64 > self.sectors {
            return Err(AtaError::OutOfRange);
        }
        let mut done = 0usize;
        while done < total {
            let n = (total - done).min(MAX_SECTORS_PER_COMMAND);
            self.start(lba + done as u64, n, false)?;
            for s in 0..n {
                wait_drq(self.chan)?;
                let base = (done + s) * SECTOR_SIZE;
                for w in 0..SECTOR_SIZE / 2 {
                    let v = buf[base + w * 2] as u16 | (buf[base + w * 2 + 1] as u16) << 8;
                    unsafe { outw(self.chan.io + DATA, v) };
                }
            }
            done += n;
        }
        unsafe {
            outb(self.chan.io + COMMAND, if self.lba48 { CMD_FLUSH_CACHE_EXT } else { CMD_FLUSH_CACHE });
        }
        wait_not_busy(self.chan)?;
        Ok(())
    }
}
