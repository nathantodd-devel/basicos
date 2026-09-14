//! Raw x86 port I/O.

#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    value
}

#[inline]
pub unsafe fn outw(port: u16, value: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
}

#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
    value
}

/// Write to an unused port to force a small delay — some ancient PIC
/// hardware needs a beat between successive command-register writes.
#[inline]
pub unsafe fn io_wait() {
    outb(0x80, 0);
}
