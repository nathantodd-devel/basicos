//! Exception handlers, IRQ handlers, and interrupt subsystem init.

#[cfg(feature = "pit-100hz")]
use super::pit;
#[cfg(feature = "keyboard")]
use super::port;
use super::{idt, pic};
#[cfg(feature = "pit-100hz")]
use crate::pr_info;
use crate::{pr_crit, pr_err, pr_warn};
use core::sync::atomic::{AtomicU64, Ordering};

/// Pushed onto the stack by the CPU before it invokes our handler.
/// Field order/types must match the hardware exactly.
#[repr(C)]
pub struct InterruptStackFrame {
    pub instruction_pointer: u64,
    pub code_segment: u64,
    pub cpu_flags: u64,
    pub stack_pointer: u64,
    pub stack_segment: u64,
}

pub static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// PIT tick rate. `log.rs` uses this to convert `TIMER_TICKS` into a
/// timestamp, so keep the two in sync if this ever changes.
///
/// 100 Hz when we've reprogrammed the PIT (`pit-100hz` feature); left at
/// the hardware's power-on default otherwise (truncated to a whole
/// number — the real rate is ~18.2 Hz, so timestamps are coarser and
/// very slightly off, but still correctly scaled).
#[cfg(feature = "pit-100hz")]
pub const TIMER_HZ: u32 = 100;
#[cfg(not(feature = "pit-100hz"))]
pub const TIMER_HZ: u32 = 18;

// --- CPU exceptions (vectors 0-31) --------------------------------------

/// If a fault came from ring 3 it's that program's bug, not the kernel's:
/// end just that process, with the status a shell shows for death by
/// `signal`, and keep going. Returns (only) for a fault in the kernel.
fn kill_if_user(frame: &InterruptStackFrame, what: &str, signal: i32) {
    #[cfg(feature = "elf-loader")]
    if frame.code_segment & 3 == 3 {
        pr_err!(
            "pid {}: {} at {:#x}, killed",
            crate::sched::current_pid(),
            what,
            frame.instruction_pointer
        );
        // We came from user mode, so no kernel lock is held and nothing
        // here can return: this frame is simply abandoned.
        enable();
        crate::sched::exit_process(128 + signal);
    }
    #[cfg(not(feature = "elf-loader"))]
    let _ = (frame, what, signal);
}

const SIGILL: i32 = 4;
const SIGBUS: i32 = 7;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "divide error", SIGFPE);
    pr_crit!(
        "EXCEPTION: DIVIDE ERROR at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!("EXCEPTION: DIVIDE ERROR at {:#x}", frame.instruction_pointer);
    halt_loop();
}

extern "x86-interrupt" fn debug_handler(frame: InterruptStackFrame) {
    pr_warn!("EXCEPTION: DEBUG at {:#x}", frame.instruction_pointer);
    // Debug exceptions are often continuable
}

extern "x86-interrupt" fn nmi_handler(frame: InterruptStackFrame) {
    pr_crit!("EXCEPTION: NMI at {:#x}", frame.instruction_pointer);
    // NMI could be hardware failure, memory parity error, etc.
    #[cfg(feature = "bad-interrupt-panic")]
    panic!("EXCEPTION: NMI at {:#x}", frame.instruction_pointer);
    halt_loop();
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    pr_err!(
        "EXCEPTION: BREAKPOINT (int3) at {:#x}",
        frame.instruction_pointer
    );
}

extern "x86-interrupt" fn overflow_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "overflow", SIGFPE);
    pr_crit!("EXCEPTION: OVERFLOW at {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!("EXCEPTION: OVERFLOW at {:#x}", frame.instruction_pointer);
    halt_loop();
}

extern "x86-interrupt" fn bound_range_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "bound range exceeded", SIGSEGV);
    pr_crit!(
        "EXCEPTION: BOUND RANGE EXCEEDED at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: BOUND RANGE EXCEEDED at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "invalid opcode", SIGILL);
    pr_crit!(
        "EXCEPTION: INVALID OPCODE at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: INVALID OPCODE at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn device_not_available_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "FPU instruction", SIGILL);
    pr_crit!(
        "EXCEPTION: DEVICE NOT AVAILABLE (FPU) at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: DEVICE NOT AVAILABLE (FPU) at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, error_code: u64) -> ! {
    pr_crit!("EXCEPTION: DOUBLE FAULT");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    pr_crit!("  RSP: {:#x}", frame.stack_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: DOUBLE FAULT
  error_code: {:#x}
  RIP: {:#x}
  RSP: {:#x}",
        error_code, frame.instruction_pointer, frame.stack_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn invalid_tss_handler(frame: InterruptStackFrame, error_code: u64) {
    pr_crit!("EXCEPTION: INVALID TSS");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: INVALID TSS
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn segment_not_present_handler(frame: InterruptStackFrame, error_code: u64) {
    kill_if_user(&frame, "segment not present", SIGSEGV);
    pr_crit!("EXCEPTION: SEGMENT NOT PRESENT");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: SEGMENT NOT PRESENT
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn stack_segment_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    kill_if_user(&frame, "stack segment fault", SIGSEGV);
    pr_crit!("EXCEPTION: STACK SEGMENT FAULT");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: STACK SEGMENT FAULT
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn general_protection_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    kill_if_user(&frame, "general protection fault", SIGSEGV);
    pr_crit!("EXCEPTION: GENERAL PROTECTION FAULT");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: GENERAL PROTECTION FAULT
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn page_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    let faulting_addr: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) faulting_addr, options(nomem, nostack, preserves_flags));
    }
    if frame.code_segment & 3 == 3 {
        pr_err!("page fault on {:#x}", faulting_addr);
    }
    kill_if_user(&frame, "segmentation fault", SIGSEGV);
    pr_crit!("EXCEPTION: PAGE FAULT");
    pr_crit!("  faulting address: {:#x}", faulting_addr);
    // No `format!` here: a fault handler shouldn't allocate (and this
    // must build without the heap).
    pr_crit!(
        "  error: {}{}{}{}{}",
        if error_code & 1 != 0 { "protection " } else { "not-present " },
        if error_code & 2 != 0 { "write " } else { "read " },
        if error_code & 4 != 0 { "user " } else { "kernel " },
        if error_code & 8 != 0 { "reserved-bit " } else { "" },
        if error_code & 16 != 0 { "instruction-fetch" } else { "" }
    );
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);

    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: PAGE FAULT\n\
         faulting address: {:#x}\n\
         error: {}\n\
         RIP: {:#x}",
        faulting_addr, error, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn x87_fpu_error_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "x87 FPU error", SIGFPE);
    pr_crit!(
        "EXCEPTION: x87 FPU ERROR at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: x87 FPU ERROR at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn alignment_check_handler(frame: InterruptStackFrame, error_code: u64) {
    kill_if_user(&frame, "alignment check", SIGBUS);
    pr_crit!("EXCEPTION: ALIGNMENT CHECK");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: ALIGNMENT CHECK
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn machine_check_handler(frame: InterruptStackFrame) -> ! {
    pr_crit!(
        "EXCEPTION: MACHINE CHECK at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: MACHINE CHECK at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn simd_fpu_error_handler(frame: InterruptStackFrame) {
    kill_if_user(&frame, "SIMD FPU error", SIGFPE);
    pr_crit!(
        "EXCEPTION: SIMD FLOATING POINT at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: SIMD FLOATING POINT at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn virtualization_handler(frame: InterruptStackFrame) {
    pr_crit!(
        "EXCEPTION: VIRTUALIZATION at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: VIRTUALIZATION at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn control_protection_handler(frame: InterruptStackFrame, error_code: u64) {
    pr_crit!("EXCEPTION: CONTROL PROTECTION");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: CONTROL PROTECTION
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn hypervisor_injection_handler(frame: InterruptStackFrame) {
    pr_crit!(
        "EXCEPTION: HYPERVISOR INJECTION at {:#x}",
        frame.instruction_pointer
    );
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        "EXCEPTION: HYPERVISOR INJECTION at {:#x}",
        frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn vmm_communication_handler(frame: InterruptStackFrame, error_code: u64) {
    pr_crit!("EXCEPTION: VMM COMMUNICATION");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: VMM COMMUNICATION
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

extern "x86-interrupt" fn security_exception_handler(frame: InterruptStackFrame, error_code: u64) {
    pr_crit!("EXCEPTION: SECURITY EXCEPTION");
    pr_crit!("  error_code: {:#x}", error_code);
    pr_crit!("  RIP: {:#x}", frame.instruction_pointer);
    #[cfg(feature = "bad-interrupt-panic")]
    panic!(
        r"EXCEPTION: SECURITY EXCEPTION
  error_code: {:#x}
  RIP: {:#x}",
        error_code, frame.instruction_pointer
    );
    halt_loop();
}

fn halt_loop() -> ! {
    loop {
        unsafe { core::arch::asm!("cli", "hlt") };
    }
}

// --- hardware IRQs (remapped to 0x20-0x2F) -------------------------------

extern "x86-interrupt" fn timer_interrupt_handler(_frame: InterruptStackFrame) {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    // EOI *before* a possible preemption below: `yield_now` might not
    // return here for a while (another task could run for many ticks
    // first), and until EOI is sent the PIC won't deliver IRQ0 again —
    // sending it after would silently stop the clock (and preemption
    // with it) until whatever we switched away from happens to be
    // scheduled back in. See `sched`'s module docs for the full picture.
    unsafe { pic::send_eoi(0) };
    #[cfg(feature = "scheduler")]
    crate::sched::yield_now();
}

#[cfg(feature = "keyboard")]
extern "x86-interrupt" fn keyboard_interrupt_handler(_frame: InterruptStackFrame) {
    let scancode = unsafe { port::inb(0x60) };
    crate::drivers::keyboard::on_scancode(scancode);
    unsafe { pic::send_eoi(1) };
}

#[cfg(feature = "serial-input")]
extern "x86-interrupt" fn serial_interrupt_handler(_frame: InterruptStackFrame) {
    crate::drivers::serial::on_interrupt();
    unsafe { pic::send_eoi(4) };
}

extern "x86-interrupt" fn spurious_irq7_handler(_frame: InterruptStackFrame) {
    // Check if this is a real IRQ or spurious
    // For spurious IRQ7, don't send EOI
    let isr = unsafe { pic::read_isr() };
    if isr & 0x80 != 0 {
        // Real IRQ7, send EOI
        unsafe { pic::send_eoi(7) };
    }
    // Spurious: do nothing
}

extern "x86-interrupt" fn spurious_irq15_handler(_frame: InterruptStackFrame) {
    // Check if this is a real IRQ or spurious
    let isr = unsafe { pic::read_isr() };
    if isr & 0x8000 != 0 {
        // Real IRQ15, send EOI to both PICs
        unsafe { pic::send_eoi(15) };
    } else {
        // Spurious IRQ15: still need to send EOI to master
        unsafe { pic::send_eoi(0) };
    }
}

// --- vector numbers -------------------------------------------------------

// CPU exceptions (0-31)
const VEC_DIVIDE_ERROR: u8 = 0;
const VEC_DEBUG: u8 = 1;
const VEC_NMI: u8 = 2;
const VEC_BREAKPOINT: u8 = 3;
const VEC_OVERFLOW: u8 = 4;
const VEC_BOUND_RANGE: u8 = 5;
const VEC_INVALID_OPCODE: u8 = 6;
const VEC_DEVICE_NOT_AVAILABLE: u8 = 7;
const VEC_DOUBLE_FAULT: u8 = 8;
// 9 is reserved (coprocessor segment overrun, obsolete)
const VEC_INVALID_TSS: u8 = 10;
const VEC_SEGMENT_NOT_PRESENT: u8 = 11;
const VEC_STACK_SEGMENT_FAULT: u8 = 12;
const VEC_GENERAL_PROTECTION: u8 = 13;
const VEC_PAGE_FAULT: u8 = 14;
// 15 is reserved
const VEC_X87_FPU_ERROR: u8 = 16;
const VEC_ALIGNMENT_CHECK: u8 = 17;
const VEC_MACHINE_CHECK: u8 = 18;
const VEC_SIMD_FPU_ERROR: u8 = 19;
const VEC_VIRTUALIZATION: u8 = 20;
const VEC_CONTROL_PROTECTION: u8 = 21;
// 22-27 are reserved
const VEC_HYPERVISOR_INJECTION: u8 = 28;
const VEC_VMM_COMMUNICATION: u8 = 29;
const VEC_SECURITY_EXCEPTION: u8 = 30;
// 31 is reserved

// Hardware IRQs (32-47 after PIC remap)
const VEC_TIMER: u8 = 32; // IRQ0
const VEC_KEYBOARD: u8 = 33; // IRQ1
const VEC_CASCADE: u8 = 34; // IRQ2 (cascade, never fires)
const VEC_COM2: u8 = 35; // IRQ3
const VEC_COM1: u8 = 36; // IRQ4
const VEC_LPT2: u8 = 37; // IRQ5
const VEC_FLOPPY: u8 = 38; // IRQ6
const VEC_SPURIOUS7: u8 = 39; // IRQ7 (often spurious)
const VEC_RTC: u8 = 40; // IRQ8
const VEC_IRQ9: u8 = 41; // IRQ9
const VEC_IRQ10: u8 = 42; // IRQ10
const VEC_IRQ11: u8 = 43; // IRQ11
const VEC_PS2_MOUSE: u8 = 44; // IRQ12
const VEC_FPU: u8 = 45; // IRQ13
const VEC_ATA_PRIMARY: u8 = 46; // IRQ14
const VEC_ATA_SECONDARY: u8 = 47; // IRQ15 (can be spurious)

/// Helper macro to convert function pointer to u64 safely.
macro_rules! handler_addr {
    ($handler:expr) => {
        $handler as *const () as u64
    };
}

/// Set up the IDT, remap the PIC, unmask timer+keyboard+serial, and enable
/// interrupts. Call once from kernel_main after VGA is ready.
pub fn init() {
    unsafe {
        // CPU exceptions
        idt::set_handler(VEC_DIVIDE_ERROR, handler_addr!(divide_error_handler));
        idt::set_handler(VEC_DEBUG, handler_addr!(debug_handler));
        idt::set_handler(VEC_NMI, handler_addr!(nmi_handler));
        idt::set_handler(VEC_BREAKPOINT, handler_addr!(breakpoint_handler));
        idt::set_handler(VEC_OVERFLOW, handler_addr!(overflow_handler));
        idt::set_handler(VEC_BOUND_RANGE, handler_addr!(bound_range_handler));
        idt::set_handler(VEC_INVALID_OPCODE, handler_addr!(invalid_opcode_handler));
        idt::set_handler(
            VEC_DEVICE_NOT_AVAILABLE,
            handler_addr!(device_not_available_handler),
        );
        idt::set_handler(VEC_DOUBLE_FAULT, handler_addr!(double_fault_handler));
        idt::set_handler(VEC_INVALID_TSS, handler_addr!(invalid_tss_handler));
        idt::set_handler(
            VEC_SEGMENT_NOT_PRESENT,
            handler_addr!(segment_not_present_handler),
        );
        idt::set_handler(
            VEC_STACK_SEGMENT_FAULT,
            handler_addr!(stack_segment_fault_handler),
        );
        idt::set_handler(
            VEC_GENERAL_PROTECTION,
            handler_addr!(general_protection_fault_handler),
        );
        idt::set_handler(VEC_PAGE_FAULT, handler_addr!(page_fault_handler));
        idt::set_handler(VEC_X87_FPU_ERROR, handler_addr!(x87_fpu_error_handler));
        idt::set_handler(VEC_ALIGNMENT_CHECK, handler_addr!(alignment_check_handler));
        idt::set_handler(VEC_MACHINE_CHECK, handler_addr!(machine_check_handler));
        idt::set_handler(VEC_SIMD_FPU_ERROR, handler_addr!(simd_fpu_error_handler));
        idt::set_handler(VEC_VIRTUALIZATION, handler_addr!(virtualization_handler));
        idt::set_handler(
            VEC_CONTROL_PROTECTION,
            handler_addr!(control_protection_handler),
        );
        idt::set_handler(
            VEC_HYPERVISOR_INJECTION,
            handler_addr!(hypervisor_injection_handler),
        );
        idt::set_handler(
            VEC_VMM_COMMUNICATION,
            handler_addr!(vmm_communication_handler),
        );
        idt::set_handler(
            VEC_SECURITY_EXCEPTION,
            handler_addr!(security_exception_handler),
        );

        // Hardware IRQs
        idt::set_handler(VEC_TIMER, handler_addr!(timer_interrupt_handler));
        #[cfg(feature = "keyboard")]
        idt::set_handler(VEC_KEYBOARD, handler_addr!(keyboard_interrupt_handler));
        #[cfg(feature = "serial-input")]
        idt::set_handler(VEC_COM1, handler_addr!(serial_interrupt_handler));
        idt::set_handler(VEC_SPURIOUS7, handler_addr!(spurious_irq7_handler));
        idt::set_handler(VEC_ATA_SECONDARY, handler_addr!(spurious_irq15_handler));

        idt::load();

        pic::remap(0x20, 0x28);

        // Mask bits: 1 = disabled, 0 = enabled. IRQ0 (timer) is always
        // on; IRQ1/IRQ4 only if their driver is actually compiled in —
        // otherwise we'd take an interrupt with no handler installed
        // for it (unmapped IDT entry -> #GP -> halt_loop).
        let mut master_mask: u8 = 0b1111_1110; // unmask IRQ0
        #[cfg(feature = "keyboard")]
        {
            master_mask &= !(1 << 1); // unmask IRQ1
        }
        #[cfg(feature = "serial-input")]
        {
            master_mask &= !(1 << 4); // unmask IRQ4
        }
        pic::set_masks(master_mask, 0xFF);

        #[cfg(feature = "pit-100hz")]
        {
            pit::set_frequency(TIMER_HZ);
            pr_info!("PIT: reprogrammed channel 0 to {} Hz", TIMER_HZ);
        }

        core::arch::asm!("sti");
    }
}

/// Disable interrupts and return whether they were previously enabled.
#[inline]
pub fn disable() -> bool {
    let flags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) flags,
            options(nomem, preserves_flags)
        );
    }
    flags & 0x200 != 0 // IF flag
}

/// Enable interrupts.
#[inline]
pub fn enable() {
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Restore interrupts to a previous state.
#[inline]
pub fn restore(was_enabled: bool) {
    if was_enabled {
        enable();
    }
}

/// Check if interrupts are enabled.
#[inline]
pub fn are_enabled() -> bool {
    let flags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            out(reg) flags,
            options(nomem, preserves_flags)
        );
    }
    flags & 0x200 != 0
}
