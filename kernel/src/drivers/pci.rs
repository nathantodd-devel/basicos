//! PCI configuration-space access and bus enumeration, via the legacy
//! CF8/CFC I/O-port mechanism (the "configuration mechanism #1" every
//! x86 chipset since the 90s supports). Enough to find a device, read
//! its BARs, and enable I/O + bus mastering — no MSI, no bridges'
//! secondary-bus setup (firmware already did that).

use crate::arch::port::{inl, outl};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

pub const COMMAND: u8 = 0x04;
pub const CMD_IO_SPACE: u16 = 1 << 0;
pub const CMD_MEM_SPACE: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

/// A base address register's decoded contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    Io(u16),
    Mem(u64),
    None,
}

fn address(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    1 << 31 | (bus as u32) << 16 | (dev as u32) << 11 | (func as u32) << 8 | (offset as u32 & 0xFC)
}

pub fn read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, dev, func, offset));
        inl(CONFIG_DATA)
    }
}

pub fn write32(bus: u8, dev: u8, func: u8, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, dev, func, offset));
        outl(CONFIG_DATA, value);
    }
}

fn read16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    (read32(bus, dev, func, offset) >> ((offset & 2) * 8)) as u16
}

impl PciDevice {
    pub fn read_config(&self, offset: u8) -> u32 {
        read32(self.bus, self.dev, self.func, offset)
    }

    pub fn write_config(&self, offset: u8, value: u32) {
        write32(self.bus, self.dev, self.func, offset, value)
    }

    /// BAR `index` (0-5) of a type-0 (endpoint) header.
    pub fn bar(&self, index: u8) -> Bar {
        let raw = self.read_config(0x10 + index * 4);
        if raw == 0 {
            Bar::None
        } else if raw & 1 != 0 {
            Bar::Io((raw & !0x3) as u16)
        } else if (raw >> 1) & 3 == 2 {
            // 64-bit memory BAR: the next register holds the high half.
            let hi = self.read_config(0x10 + (index + 1) * 4) as u64;
            Bar::Mem(hi << 32 | (raw & !0xF) as u64)
        } else {
            Bar::Mem((raw & !0xF) as u64)
        }
    }

    /// The legacy interrupt line (the PIC IRQ number firmware routed
    /// this device's INTx pin to).
    pub fn interrupt_line(&self) -> u8 {
        self.read_config(0x3C) as u8
    }

    /// OR bits into the command register (I/O space, memory space, bus
    /// mastering — a device can't DMA until the last is set).
    pub fn enable(&self, bits: u16) {
        // The command register is the low half of a dword shared with
        // the status register, whose bits are write-1-to-clear — so
        // write back *only* the command half, with the status half as
        // zeros ("clear nothing"), rather than echoing the whole dword
        // we read and clearing any pending status flag by accident.
        let command = (self.read_config(COMMAND) as u16) | bits;
        self.write_config(COMMAND, command as u32);
    }
}

/// Call `f` for every PCI function present.
pub fn enumerate(mut f: impl FnMut(PciDevice)) {
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            let vendor = read16(bus, dev, 0, 0);
            if vendor == 0xFFFF {
                continue;
            }
            // Header type bit 7: multi-function device (functions 1-7).
            let multi = read32(bus, dev, 0, 0x0C) >> 16 & 0x80 != 0;
            for func in 0..if multi { 8 } else { 1 } {
                let vendor = read16(bus, dev, func, 0);
                if vendor == 0xFFFF {
                    continue;
                }
                let class_dword = read32(bus, dev, func, 0x08);
                f(PciDevice {
                    bus,
                    dev,
                    func,
                    vendor,
                    device: read16(bus, dev, func, 2),
                    class: (class_dword >> 24) as u8,
                    subclass: (class_dword >> 16) as u8,
                    prog_if: (class_dword >> 8) as u8,
                });
            }
        }
    }
}

/// First device with this vendor/device ID.
pub fn find(vendor: u16, device: u16) -> Option<PciDevice> {
    let mut found = None;
    enumerate(|d| {
        if found.is_none() && d.vendor == vendor && d.device == device {
            found = Some(d);
        }
    });
    found
}
