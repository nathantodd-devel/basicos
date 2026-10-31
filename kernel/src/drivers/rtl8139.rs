//! Realtek RTL8139 Fast Ethernet driver — the NIC QEMU emulates with
//! `-device rtl8139`, chosen because it's about as simple as a real
//! network chip gets: one receive ring, four transmit slots, port I/O.
//!
//! Polled, not interrupt-driven: [`NetDevice::recv`] checks the chip's
//! "buffer empty" bit, and the `net` task calls it in a loop. (The IRQ
//! line is discoverable via PCI, but nothing here needs the latency yet.)
//!
//! DMA buffers come from the frame allocator. The kernel identity-maps
//! all of physical memory it tracks, so a buffer's virtual address *is*
//! the physical address the chip is programmed with.

use super::pci::{self, Bar, PciDevice};
use crate::arch::port::{inb, inl, inw, outb, outl, outw};
use crate::mem::frame::{self, FRAME_SIZE};
use basicos_net::stack::{DeviceError, NetDevice};
use basicos_net::MacAddr;

pub const VENDOR_ID: u16 = 0x10EC;
pub const DEVICE_ID: u16 = 0x8139;

// Register offsets from the I/O base.
const IDR0: u16 = 0x00; // MAC address, 6 bytes
const TSD0: u16 = 0x10; // transmit status of descriptor 0..3 (stride 4)
const TSAD0: u16 = 0x20; // transmit start address of descriptor 0..3
const RBSTART: u16 = 0x30; // receive buffer start (physical)
const CMD: u16 = 0x37;
const CAPR: u16 = 0x38; // current address of packet read
const IMR: u16 = 0x3C;
const ISR: u16 = 0x3E;
const RCR: u16 = 0x44;
const CONFIG1: u16 = 0x52;

const CMD_BUFE: u8 = 1 << 0; // rx buffer empty
const CMD_TE: u8 = 1 << 2;
const CMD_RE: u8 = 1 << 3;
const CMD_RESET: u8 = 1 << 4;

const TSD_OWN: u32 = 1 << 13; // set once the chip has DMA'd the buffer out

// Receive config: accept physical-match (APM), multicast (AM) and
// broadcast (AB); WRAP so a packet crossing the end of the ring is laid
// out contiguously in the overflow area instead of split; unlimited DMA
// burst and no FIFO threshold.
const RCR_VALUE: u32 = 0x02 | 0x04 | 0x08 | 0x80 | (7 << 8) | (7 << 13);

/// The receive ring is 8 KiB (RBLEN=0) + 16 bytes of header slack + up
/// to one maximum frame of overflow room for WRAP mode.
const RX_RING_LEN: usize = 8192;
const RX_BUF_FRAMES: usize = 3; // 12 KiB >= 8192 + 16 + 1518
const TX_SLOTS: usize = 4;
const TX_SLOT_LEN: usize = 2048; // >= max frame (1514) with room to spare
const MAX_FRAME: usize = 1514;
const MIN_FRAME: usize = 60; // the chip won't pad short frames itself

const SPIN_LIMIT: u32 = 1_000_000;

pub struct Rtl8139 {
    io: u16,
    mac: MacAddr,
    rx_buf: usize,
    /// Read offset into the ring.
    rx_off: usize,
    tx_buf: usize,
    tx_cur: usize,
}

impl Rtl8139 {
    /// Find the NIC on the PCI bus and bring it up. `None` if there isn't
    /// one (or it can't be initialized).
    pub fn probe() -> Option<Rtl8139> {
        let pci = pci::find(VENDOR_ID, DEVICE_ID)?;
        Self::init(pci)
    }

    fn init(pci: PciDevice) -> Option<Rtl8139> {
        let Bar::Io(io) = pci.bar(0) else { return None };
        // Port I/O to talk to it, bus mastering so it can DMA.
        pci.enable(pci::CMD_IO_SPACE | pci::CMD_BUS_MASTER);

        unsafe {
            outb(io + CONFIG1, 0); // power on
            outb(io + CMD, CMD_RESET);
            let mut spins = 0;
            while inb(io + CMD) & CMD_RESET != 0 {
                spins += 1;
                if spins > SPIN_LIMIT {
                    return None;
                }
            }
        }

        let rx = frame::allocate_contiguous(RX_BUF_FRAMES)?.start_address();
        let tx = frame::allocate_contiguous(TX_SLOTS * TX_SLOT_LEN / FRAME_SIZE)?.start_address();
        // The chip's DMA addresses are 32-bit.
        if rx + RX_BUF_FRAMES * FRAME_SIZE > u32::MAX as usize || tx + TX_SLOTS * TX_SLOT_LEN > u32::MAX as usize {
            return None;
        }
        unsafe {
            core::ptr::write_bytes(rx as *mut u8, 0, RX_BUF_FRAMES * FRAME_SIZE);
            core::ptr::write_bytes(tx as *mut u8, 0, TX_SLOTS * TX_SLOT_LEN);
        }

        let mut mac = [0u8; 6];
        unsafe {
            for (i, b) in mac.iter_mut().enumerate() {
                *b = inb(io + IDR0 + i as u16);
            }
            outl(io + RBSTART, rx as u32);
            outw(io + IMR, 0); // polled: no interrupts
            outl(io + RCR, RCR_VALUE);
            outb(io + CMD, CMD_RE | CMD_TE);
        }

        Some(Rtl8139 {
            io,
            mac: MacAddr(mac),
            rx_buf: rx,
            rx_off: 0,
            tx_buf: tx,
            tx_cur: 0,
        })
    }

    pub fn io_base(&self) -> u16 {
        self.io
    }

    /// Give up on whatever's in the ring and start fresh — for a
    /// malformed packet header, where there's no trustworthy length to
    /// skip by.
    fn reset_rx(&mut self) {
        unsafe {
            outb(self.io + CMD, CMD_TE); // RE off
            core::ptr::write_bytes(self.rx_buf as *mut u8, 0, RX_BUF_FRAMES * FRAME_SIZE);
            outl(self.io + RBSTART, self.rx_buf as u32);
            outb(self.io + CMD, CMD_RE | CMD_TE);
            outl(self.io + RCR, RCR_VALUE);
        }
        self.rx_off = 0;
    }
}

impl NetDevice for Rtl8139 {
    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), DeviceError> {
        if frame.len() > MAX_FRAME {
            return Err(DeviceError::TooLong);
        }
        let slot = self.tx_cur;
        let tsd = self.io + TSD0 + 4 * slot as u16;
        // A slot is ours again once the chip sets OWN after DMA'ing it out.
        let mut spins = 0;
        while unsafe { inl(tsd) } & TSD_OWN == 0 {
            spins += 1;
            if spins > SPIN_LIMIT {
                return Err(DeviceError::Busy);
            }
        }

        let buf = self.tx_buf + slot * TX_SLOT_LEN;
        let len = frame.len().max(MIN_FRAME);
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len());
            core::ptr::write_bytes((buf + frame.len()) as *mut u8, 0, len - frame.len());
            outl(self.io + TSAD0 + 4 * slot as u16, buf as u32);
            // Writing the size (with OWN clear) is what starts the transmit.
            outl(tsd, len as u32);
        }
        self.tx_cur = (slot + 1) % TX_SLOTS;
        Ok(())
    }

    fn recv(&mut self, out: &mut [u8]) -> Option<usize> {
        if unsafe { inb(self.io + CMD) } & CMD_BUFE != 0 {
            return None;
        }
        // Each packet in the ring is: u16 status, u16 length (including
        // the 4-byte CRC), then the frame.
        let base = self.rx_buf + self.rx_off;
        let (status, len) = unsafe {
            (
                core::ptr::read_volatile(base as *const u16),
                core::ptr::read_volatile((base + 2) as *const u16) as usize,
            )
        };
        let rok = status & 1 != 0;
        if !rok || !(4 + 14..=MAX_FRAME + 4 + 4).contains(&len) {
            self.reset_rx();
            return None;
        }

        let frame_len = len - 4; // strip CRC
        let copied = if frame_len <= out.len() {
            unsafe {
                core::ptr::copy_nonoverlapping((base + 4) as *const u8, out.as_mut_ptr(), frame_len);
            }
            Some(frame_len)
        } else {
            None // caller's buffer too small: drop it
        };

        // Advance past this packet (4-byte aligned) and tell the chip how
        // far we've read; it wants CAPR 16 bytes behind the real offset.
        self.rx_off = (self.rx_off + len + 4 + 3) & !3;
        self.rx_off %= RX_RING_LEN;
        unsafe {
            outw(self.io + CAPR, (self.rx_off as u16).wrapping_sub(16));
            outw(self.io + ISR, inw(self.io + ISR)); // ack (write-1-to-clear)
        }
        copied
    }
}
