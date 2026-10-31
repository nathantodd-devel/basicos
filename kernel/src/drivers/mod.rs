//! Hardware drivers: VGA, serial, keyboard, ATA disk, PCI, network.

#[cfg(feature = "storage")]
pub mod ata;
#[cfg(feature = "keyboard")]
pub mod keyboard;
#[cfg(feature = "net")]
pub mod pci;
#[cfg(feature = "net")]
pub mod rtl8139;
#[cfg(any(feature = "usermode-demo", feature = "storage"))]
pub mod rtc;
pub mod serial;
pub mod vga;
