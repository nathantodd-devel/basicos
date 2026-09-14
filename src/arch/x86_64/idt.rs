//! Interrupt Descriptor Table.
//!
//! Hand-rolled 64-bit IDT: 256 entries, each a 16-byte gate descriptor.
//! We don't use the `x86_64` crate here to stay consistent with the rest
//! of basicos, which talks to the hardware directly.

pub const ENTRY_COUNT: usize = 256;

/// Selector for our 64-bit kernel code segment.
use super::gdt::KERNEL_CODE_SELECTOR as CODE_SELECTOR;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// `handler` is the interrupt handler's address, cast to u64.
    fn set(&mut self, handler: u64) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = CODE_SELECTOR;
        self.ist = 0; // not using a separate interrupt stack yet
        // present (bit 7) | DPL 00 (bits 5-6) | 64-bit interrupt gate (0xE)
        self.type_attr = 0x8E;
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; ENTRY_COUNT] = [IdtEntry::missing(); ENTRY_COUNT];

/// Install a handler at the given interrupt vector (0-255).
/// `handler` must be a fn pointer with the `extern "x86-interrupt"` ABI,
/// cast to u64 at the call site.
pub unsafe fn set_handler(vector: u8, handler: u64) {
    IDT[vector as usize].set(handler);
}

/// Load the IDT with LIDT. Call once, after all handlers are installed.
pub unsafe fn load() {
    let ptr = IdtPointer {
        limit: (core::mem::size_of::<[IdtEntry; ENTRY_COUNT]>() - 1) as u16,
        base: core::ptr::addr_of!(IDT) as u64,
    };
    core::arch::asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
}
