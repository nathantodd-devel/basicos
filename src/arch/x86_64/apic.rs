//! Local APIC (Advanced Programmable Interrupt Controller).
//!
//! The APIC is the modern interrupt controller used in x86_64 systems.
//! Each CPU core has its own Local APIC, and there's an I/O APIC for
//! routing external interrupts. This module handles the Local APIC.
//!
//! Key features over legacy 8259 PIC:
//! - Per-CPU interrupt handling (required for SMP)
//! - Built-in timer with configurable frequency
//! - Inter-processor interrupts (IPIs)
//! - More interrupt vectors (256 vs 16)

use super::port::outb;
use crate::pr_info;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default physical address of the Local APIC MMIO registers.
/// This can be relocated via the IA32_APIC_BASE MSR.
const APIC_BASE_DEFAULT: u64 = 0xFEE0_0000;

/// MSR for APIC base address and enable bit.
const IA32_APIC_BASE_MSR: u32 = 0x1B;

// APIC register offsets (from base address)
const APIC_ID: u32 = 0x020;
const APIC_VERSION: u32 = 0x030;
const APIC_TPR: u32 = 0x080; // Task Priority Register
const APIC_EOI: u32 = 0x0B0; // End of Interrupt
const APIC_SVR: u32 = 0x0F0; // Spurious Interrupt Vector Register
const APIC_ESR: u32 = 0x280; // Error Status Register
const APIC_ICR_LOW: u32 = 0x300; // Interrupt Command Register (low)
const APIC_ICR_HIGH: u32 = 0x310; // Interrupt Command Register (high)
const APIC_LVT_TIMER: u32 = 0x320; // LVT Timer Register
const APIC_LVT_LINT0: u32 = 0x350; // LVT LINT0 Register
const APIC_LVT_LINT1: u32 = 0x360; // LVT LINT1 Register
const APIC_LVT_ERROR: u32 = 0x370; // LVT Error Register
const APIC_TIMER_INIT: u32 = 0x380; // Timer Initial Count
const APIC_TIMER_CURRENT: u32 = 0x390; // Timer Current Count
const APIC_TIMER_DIV: u32 = 0x3E0; // Timer Divide Configuration

// SVR bits
const SVR_ENABLE: u32 = 1 << 8;
const SVR_VECTOR_MASK: u32 = 0xFF;

// LVT bits
const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;

// Timer divider values
const TIMER_DIV_1: u32 = 0b1011;
const TIMER_DIV_2: u32 = 0b0000;
const TIMER_DIV_4: u32 = 0b0001;
const TIMER_DIV_8: u32 = 0b0010;
const TIMER_DIV_16: u32 = 0b0011;
const TIMER_DIV_32: u32 = 0b1000;
const TIMER_DIV_64: u32 = 0b1001;
const TIMER_DIV_128: u32 = 0b1010;

/// Interrupt vectors for APIC interrupts
pub const APIC_TIMER_VECTOR: u8 = 48;
pub const APIC_ERROR_VECTOR: u8 = 49;
pub const APIC_SPURIOUS_VECTOR: u8 = 0xFF;

/// Whether the APIC has been initialized.
static APIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// APIC base address (may differ from default if relocated).
static APIC_BASE: AtomicU64 = AtomicU64::new(APIC_BASE_DEFAULT);

/// Ticks per second of the APIC timer (calibrated at init).
static APIC_TIMER_FREQUENCY: AtomicU64 = AtomicU64::new(0);

/// Read from an APIC register.
unsafe fn read_reg(offset: u32) -> u32 {
    let base = APIC_BASE.load(Ordering::Relaxed);
    read_volatile((base + offset as u64) as *const u32)
}

/// Write to an APIC register.
unsafe fn write_reg(offset: u32, value: u32) {
    let base = APIC_BASE.load(Ordering::Relaxed);
    write_volatile((base + offset as u64) as *mut u32, value);
}

/// Read a Model-Specific Register.
unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    core::arch::asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack)
    );
    ((high as u64) << 32) | (low as u64)
}

/// Write a Model-Specific Register.
unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    core::arch::asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack)
    );
}

/// Check if the CPU supports APIC (via CPUID).
///
/// Uses the `__cpuid` intrinsic rather than a hand-rolled `asm!` block —
/// LLVM reserves RBX for its own use and refuses `out("ebx")` in inline
/// asm, so a raw CPUID leaf 1 query needs the same save/restore dance
/// `__cpuid` already does internally (see `cpu_info.rs`, which uses it
/// for exactly this reason).
pub fn is_apic_available() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    // Bit 9 of EDX indicates APIC availability
    result.edx & (1 << 9) != 0
}

/// Check if the CPU supports x2APIC mode.
#[allow(dead_code)]
pub fn is_x2apic_available() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    // Bit 21 of ECX indicates x2APIC availability
    result.ecx & (1 << 21) != 0
}

/// Get the APIC ID of the current processor.
pub fn get_apic_id() -> u32 {
    unsafe { (read_reg(APIC_ID) >> 24) & 0xFF }
}

/// Disable the legacy 8259 PIC by masking all interrupts.
/// This should be called before enabling the APIC.
unsafe fn disable_pic() {
    // Mask all IRQs on both PICs
    outb(0x21, 0xFF); // PIC1 data
    outb(0xA1, 0xFF); // PIC2 data

    // Some recommend remapping the PIC to avoid conflicts,
    // but masking is sufficient for our purposes
}

/// Initialize the Local APIC.
///
/// This enables the APIC, sets up the spurious interrupt vector,
/// and prepares the timer (but doesn't start it yet).
///
/// # Safety
/// Must be called once per CPU, from the BSP first.
pub unsafe fn init() -> Result<(), &'static str> {
    if !is_apic_available() {
        return Err("APIC not available on this CPU");
    }

    // Disable the legacy PIC first
    disable_pic();

    // Read the APIC base MSR
    let apic_base_msr = rdmsr(IA32_APIC_BASE_MSR);
    let base_addr = apic_base_msr & 0xFFFF_F000;
    APIC_BASE.store(base_addr, Ordering::Relaxed);

    // Enable the APIC if not already enabled (bit 11)
    if apic_base_msr & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, apic_base_msr | (1 << 11));
    }

    // Set the Spurious Interrupt Vector Register
    // Enable APIC (bit 8) and set spurious vector to 0xFF
    let svr = SVR_ENABLE | (APIC_SPURIOUS_VECTOR as u32);
    write_reg(APIC_SVR, svr);

    // Clear error status by writing to ESR (twice, as per Intel docs)
    write_reg(APIC_ESR, 0);
    write_reg(APIC_ESR, 0);

    // Set Task Priority to 0 (accept all interrupts)
    write_reg(APIC_TPR, 0);

    // Configure LVT entries
    // Mask the timer initially (we'll configure it separately)
    write_reg(APIC_LVT_TIMER, LVT_MASKED);

    // Set up error interrupt
    write_reg(APIC_LVT_ERROR, APIC_ERROR_VECTOR as u32);

    // Mask LINT0/LINT1 (external interrupt pins)
    write_reg(APIC_LVT_LINT0, LVT_MASKED);
    write_reg(APIC_LVT_LINT1, LVT_MASKED);

    APIC_INITIALIZED.store(true, Ordering::Release);

    let id = get_apic_id();
    let version = read_reg(APIC_VERSION) & 0xFF;
    pr_info!("Local APIC enabled: ID={}, version={:#x}", id, version);

    Ok(())
}

/// Calibrate the APIC timer using the PIT.
///
/// This measures how many APIC timer ticks occur in a known time period
/// (10ms as measured by the PIT) to determine the APIC timer frequency.
///
/// Must be called after `init()` and with interrupts disabled.
pub unsafe fn calibrate_timer() {
    const PIT_FREQ: u32 = 1193182; // PIT base frequency in Hz
    const CALIBRATION_MS: u32 = 10; // Calibration period in ms
    const PIT_DIVISOR: u16 = ((PIT_FREQ * CALIBRATION_MS) / 1000) as u16;

    // Set up the APIC timer with maximum count and divider of 16
    write_reg(APIC_TIMER_DIV, TIMER_DIV_16);
    write_reg(APIC_TIMER_INIT, 0xFFFF_FFFF);

    // Program PIT channel 2 for one-shot mode
    outb(0x61, (crate::arch::port::inb(0x61) & 0xFD) | 0x01); // Enable speaker gate
    outb(0x43, 0b10110010); // Channel 2, mode 0, lobyte/hibyte
    outb(0x42, (PIT_DIVISOR & 0xFF) as u8);
    outb(0x42, ((PIT_DIVISOR >> 8) & 0xFF) as u8);

    // Reset PIT counter by toggling the gate
    let tmp = crate::arch::port::inb(0x61) & 0xFE;
    outb(0x61, tmp);
    outb(0x61, tmp | 1);

    // Wait for PIT to count down (poll bit 5 of port 0x61)
    while crate::arch::port::inb(0x61) & 0x20 == 0 {
        core::hint::spin_loop();
    }

    // Read APIC timer count
    let apic_ticks = 0xFFFF_FFFF_u32 - read_reg(APIC_TIMER_CURRENT);

    // Stop the APIC timer
    write_reg(APIC_TIMER_INIT, 0);

    // Calculate frequency: ticks_per_ms = apic_ticks / CALIBRATION_MS
    // Then ticks_per_second = ticks_per_ms * 1000
    let ticks_per_second = (apic_ticks as u64 * 1000) / CALIBRATION_MS as u64;

    // Account for the divider (we used 16)
    let actual_freq = ticks_per_second * 16;
    APIC_TIMER_FREQUENCY.store(actual_freq, Ordering::Relaxed);

    pr_info!("APIC timer frequency: {} Hz", actual_freq);
}

/// Start the APIC timer in periodic mode at the given frequency (Hz).
///
/// The timer will generate interrupts at `APIC_TIMER_VECTOR`.
pub unsafe fn start_timer(frequency: u32) {
    let timer_freq = APIC_TIMER_FREQUENCY.load(Ordering::Relaxed);
    if timer_freq == 0 {
        panic!("APIC timer not calibrated");
    }

    // Calculate the initial count for the desired frequency
    // Use divider of 16 (same as calibration)
    let ticks = (timer_freq / 16) / frequency as u64;

    if ticks == 0 || ticks > 0xFFFF_FFFF {
        panic!("Invalid APIC timer frequency");
    }

    // Set up periodic mode
    write_reg(APIC_TIMER_DIV, TIMER_DIV_16);
    write_reg(APIC_LVT_TIMER, LVT_TIMER_PERIODIC | (APIC_TIMER_VECTOR as u32));
    write_reg(APIC_TIMER_INIT, ticks as u32);
}

/// Stop the APIC timer.
#[allow(dead_code)]
pub unsafe fn stop_timer() {
    write_reg(APIC_LVT_TIMER, LVT_MASKED);
    write_reg(APIC_TIMER_INIT, 0);
}

/// Send End-Of-Interrupt to the Local APIC.
///
/// Must be called at the end of every APIC interrupt handler.
pub unsafe fn send_eoi() {
    write_reg(APIC_EOI, 0);
}

/// Send an Inter-Processor Interrupt (IPI) to another CPU.
///
/// # Arguments
/// * `target_apic_id` - The APIC ID of the target CPU
/// * `vector` - The interrupt vector to send
#[allow(dead_code)]
pub unsafe fn send_ipi(target_apic_id: u8, vector: u8) {
    // Set destination in ICR high
    write_reg(APIC_ICR_HIGH, (target_apic_id as u32) << 24);

    // Send fixed interrupt to target
    // Delivery mode = 000 (Fixed), Level = 1, Trigger = 0 (Edge)
    write_reg(APIC_ICR_LOW, (1 << 14) | (vector as u32));

    // Wait for delivery
    while read_reg(APIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
}

/// Send an IPI to all other CPUs (broadcast excluding self).
#[allow(dead_code)]
pub unsafe fn send_ipi_all_excluding_self(vector: u8) {
    // Shorthand = 11 (All Excluding Self)
    write_reg(APIC_ICR_HIGH, 0);
    write_reg(APIC_ICR_LOW, (0b11 << 18) | (1 << 14) | (vector as u32));

    // Wait for delivery
    while read_reg(APIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
}

/// Check if the APIC is initialized.
pub fn is_initialized() -> bool {
    APIC_INITIALIZED.load(Ordering::Acquire)
}

/// Get the calibrated APIC timer frequency in Hz.
#[allow(dead_code)]
pub fn get_timer_frequency() -> u64 {
    APIC_TIMER_FREQUENCY.load(Ordering::Relaxed)
}
