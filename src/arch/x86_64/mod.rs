//! x86_64-specific code: GDT, IDT, interrupts, CPU info, port I/O, PIC, syscalls.

#[cfg(feature = "apic")]
pub mod apic;
pub mod cpu_info;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod pic;
#[cfg(feature = "pit-100hz")]
pub mod pit;
pub mod port;
#[cfg(feature = "usermode-demo")]
pub mod syscall;
