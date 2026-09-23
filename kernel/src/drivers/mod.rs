//! Hardware drivers: VGA, serial, keyboard, ATA disk.

#[cfg(feature = "storage")]
pub mod ata;
#[cfg(feature = "keyboard")]
pub mod keyboard;
pub mod serial;
pub mod vga;
