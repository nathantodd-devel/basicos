//! Legacy 8259 Programmable Interrupt Controller.
//!
//! By default the two cascaded 8259 chips fire IRQ0-15 on interrupt
//! vectors 0x00-0x0F, which collide head-on with CPU exceptions (divide
//! error, breakpoint, etc. also live at 0x00-0x1F). We remap them up to
//! 0x20-0x2F before enabling interrupts.

use super::port::{inb, io_wait, outb};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_ICW4: u8 = 0x01; // ICW4 will be present
const ICW1_INIT: u8 = 0x10; // initialization request
const ICW4_8086: u8 = 0x01; // 8086/88 mode

const PIC_EOI: u8 = 0x20;

/// Remap the master PIC's IRQs to start at `offset1` and the slave's to
/// start at `offset2`. Standard choice: offset1 = 0x20 (32), offset2 = 0x28 (40).
pub unsafe fn remap(offset1: u8, offset2: u8) {
    // ICW1: start initialization sequence (cascade mode, edge triggered)
    outb(PIC1_CMD, ICW1_INIT | ICW1_ICW4);
    io_wait();
    outb(PIC2_CMD, ICW1_INIT | ICW1_ICW4);
    io_wait();

    // ICW2: vector offsets
    outb(PIC1_DATA, offset1);
    io_wait();
    outb(PIC2_DATA, offset2);
    io_wait();

    // ICW3: tell master there's a slave PIC at IRQ2 (0000_0100),
    // tell slave its cascade identity (0000_0010)
    outb(PIC1_DATA, 4);
    io_wait();
    outb(PIC2_DATA, 2);
    io_wait();

    // ICW4: 8086 mode
    outb(PIC1_DATA, ICW4_8086);
    io_wait();
    outb(PIC2_DATA, ICW4_8086);
    io_wait();
}

/// Set the interrupt mask on each PIC. A `1` bit means that IRQ is
/// masked (disabled); a `0` bit means it's enabled.
pub unsafe fn set_masks(mask1: u8, mask2: u8) {
    outb(PIC1_DATA, mask1);
    outb(PIC2_DATA, mask2);
}

/// Must be called at the end of every hardware interrupt handler, or the
/// PIC will never send you another interrupt on that line (or any lower
/// line, on the master).
pub unsafe fn send_eoi(irq: u8) {
    if irq >= 8 {
        outb(PIC2_CMD, PIC_EOI);
    }
    outb(PIC1_CMD, PIC_EOI);
}

#[allow(dead_code)]
pub unsafe fn get_masks() -> (u8, u8) {
    (inb(PIC1_DATA), inb(PIC2_DATA))
}

/// Read the In-Service Register (ISR) from both PICs.
/// Returns a 16-bit value where bits 0-7 are master and bits 8-15 are slave.
/// Used to detect spurious IRQs.
pub unsafe fn read_isr() -> u16 {
    // OCW3: read ISR (0x0B selects ISR, 0x0A selects IRR)
    outb(PIC1_CMD, 0x0B);
    outb(PIC2_CMD, 0x0B);
    let master = inb(PIC1_CMD) as u16;
    let slave = inb(PIC2_CMD) as u16;
    master | (slave << 8)
}

/// Read the Interrupt Request Register (IRR) from both PICs.
/// Returns a 16-bit value showing pending interrupt requests.
#[allow(dead_code)]
pub unsafe fn read_irr() -> u16 {
    outb(PIC1_CMD, 0x0A);
    outb(PIC2_CMD, 0x0A);
    let master = inb(PIC1_CMD) as u16;
    let slave = inb(PIC2_CMD) as u16;
    master | (slave << 8)
}

/// Mask (disable) a specific IRQ line.
pub unsafe fn mask_irq(irq: u8) {
    let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
    let bit = if irq < 8 { irq } else { irq - 8 };
    let value = inb(port) | (1 << bit);
    outb(port, value);
}

/// Unmask (enable) a specific IRQ line.
pub unsafe fn unmask_irq(irq: u8) {
    let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
    let bit = if irq < 8 { irq } else { irq - 8 };
    let value = inb(port) & !(1 << bit);
    outb(port, value);
}
