//! Global Descriptor Table + Task State Segment.
//!
//! `entrypoint.s` builds a bare-bones GDT (null + one 64-bit code segment)
//! just to get the CPU into long mode. This module replaces it with the
//! "real" GDT — kernel code/data, user code/data, and a TSS — early in
//! `kernel_main`, before anything tries to reach ring 3.
//!
//! The segment order below is not arbitrary: `SYSCALL`/`SYSRET` (planned
//! next) derive their segment selectors from `IA32_STAR` by fixed offsets
//! from kernel-code and (kernel-data - 8), which is why user data comes
//! *before* user code here. `jump_to_ring3` already depends on this same
//! layout, so if it ever changes, check both.

use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_DATA_SELECTOR: u16 = 0x18; // RPL 3 is OR'd in at use sites
pub const USER_CODE_SELECTOR: u16 = 0x20; // RPL 3 is OR'd in at use sites
const TSS_SELECTOR: u16 = 0x28;

const GDT_ENTRIES: usize = 7; // null, kcode, kdata, udata, ucode, tss (2 slots)

// --- descriptor flag bits (64-bit code/data segment access byte + flags) ---

const PRESENT: u64 = 1 << 47;
const USER_SEGMENT: u64 = 1 << 44; // S bit: 1 = code/data, 0 = system segment
const EXECUTABLE: u64 = 1 << 43;
const WRITABLE: u64 = 1 << 41; // for data segments
const DPL_RING3: u64 = 3 << 45;
const LONG_MODE: u64 = 1 << 53; // code segments only

const KERNEL_CODE_DESC: u64 = PRESENT | USER_SEGMENT | EXECUTABLE | LONG_MODE;
const KERNEL_DATA_DESC: u64 = PRESENT | USER_SEGMENT | WRITABLE;
const USER_DATA_DESC: u64 = PRESENT | USER_SEGMENT | WRITABLE | DPL_RING3;
const USER_CODE_DESC: u64 = PRESENT | USER_SEGMENT | EXECUTABLE | LONG_MODE | DPL_RING3;

// --- Task State Segment --------------------------------------------------

/// x86_64 TSS. In long mode this no longer holds per-task register state —
/// its only jobs for us are: RSP0 (stack to switch to when an interrupt or
/// exception takes us from ring 3 back to ring 0) and, later, the IST
/// stacks for faults we want on a guaranteed-good stack (double fault).
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: size_of::<Tss>() as u16, // == limit + 1 -> no I/O permission bitmap
        }
    }
}

/// Kernel stack the CPU switches to (via TSS.RSP0) on any interrupt or
/// exception that changes privilege level from 3 to 0. Separate from the
/// boot stack in entrypoint.s, which kernel_main itself runs on.
const KERNEL_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct KernelStack([u8; KERNEL_STACK_SIZE]);

static mut KERNEL_STACK: KernelStack = KernelStack([0; KERNEL_STACK_SIZE]);
static mut TSS: Tss = Tss::new();
static mut GDT: [u64; GDT_ENTRIES] = [0; GDT_ENTRIES];

/// Build the 16-byte system-segment descriptor for the TSS, split into the
/// two u64 GDT slots it occupies.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<Tss>() - 1) as u64;

    let mut low: u64 = 0;
    low |= limit & 0xFFFF; // limit[15:0]
    low |= (base & 0xFF_FFFF) << 16; // base[23:0]
    low |= 0x89 << 40; // present, DPL0, type=0x9 (64-bit TSS, available)
    low |= ((limit >> 16) & 0xF) << 48; // limit[19:16]
    low |= ((base >> 24) & 0xFF) << 56; // base[31:24]

    let high = (base >> 32) & 0xFFFF_FFFF; // base[63:32]

    (low, high)
}

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Reload CS via the standard "push selector + return address, retfq"
/// trick — there's no way to `mov` into CS directly.
unsafe fn reload_cs(selector: u16) {
    asm!(
        "push {sel}",
        "lea {tmp}, [rip + 2f]",
        "push {tmp}",
        "retfq",
        "2:",
        sel = in(reg) selector as u64,
        tmp = lateout(reg) _,
        options(preserves_flags)
    );
}

unsafe fn reload_data_segments(selector: u16) {
    asm!(
        "mov ds, {0:x}",
        "mov es, {0:x}",
        "mov ss, {0:x}",
        "mov fs, {0:x}",
        "mov gs, {0:x}",
        in(reg) selector,
        options(nostack, preserves_flags)
    );
}

/// Build and load the real GDT + TSS. Call once from kernel_main, before
/// anything that might reach ring 3.
pub unsafe fn init() {
    let stack_top = core::ptr::addr_of!(KERNEL_STACK.0) as u64 + KERNEL_STACK_SIZE as u64;
    TSS.rsp[0] = stack_top;

    let tss_addr = core::ptr::addr_of!(TSS) as u64;
    let (tss_low, tss_high) = tss_descriptor(tss_addr);

    GDT[0] = 0;
    GDT[1] = KERNEL_CODE_DESC;
    GDT[2] = KERNEL_DATA_DESC;
    GDT[3] = USER_DATA_DESC;
    GDT[4] = USER_CODE_DESC;
    GDT[5] = tss_low;
    GDT[6] = tss_high;

    let ptr = GdtPointer {
        limit: (size_of::<[u64; GDT_ENTRIES]>() - 1) as u16,
        base: core::ptr::addr_of!(GDT) as u64,
    };
    asm!("lgdt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));

    reload_cs(KERNEL_CODE_SELECTOR);
    reload_data_segments(KERNEL_DATA_SELECTOR);

    asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
}

/// Point `TSS.RSP0` at a different stack — the one the CPU switches to on
/// any interrupt/exception that takes a ring-3 program back to ring 0.
/// The scheduler calls this on every switch *into* a task (see `sched`'s
/// module docs on why ring-3 tasks need their own kernel stack rather
/// than sharing the single boot-time one this module used to hand out).
pub fn set_kernel_stack(rsp0: u64) {
    unsafe {
        TSS.rsp[0] = rsp0;
    }
}

/// Build an `iretq` frame and drop into ring 3 at `entry`, running on
/// `user_stack_top`. Never returns to the caller — there is no ring-0
/// control flow left once this executes; ring 0 resumes only when an
/// interrupt or exception fires, switching back in via TSS.RSP0.
///
/// Note: this doesn't reload DS/ES/FS/GS to user selectors first. In long
/// mode, ordinary data accesses aren't privilege-checked against those
/// segment registers (only CS and SS matter) — the real enforcement is the
/// page tables' user/supervisor bit, so this is fine as-is.
pub unsafe fn jump_to_ring3(entry: extern "C" fn() -> !, user_stack_top: u64) -> ! {
    let user_cs = (USER_CODE_SELECTOR | 3) as u64;
    let user_ss = (USER_DATA_SELECTOR | 3) as u64;
    let rflags: u64 = 0x202; // bit 1 reserved-as-1, IF set so user mode still takes interrupts

    asm!(
        "push {ss}",
        "push {stack}",
        "push {flags}",
        "push {cs}",
        "push {entry}",
        "iretq",
        ss = in(reg) user_ss,
        stack = in(reg) user_stack_top,
        flags = in(reg) rflags,
        cs = in(reg) user_cs,
        entry = in(reg) entry as u64,
        options(noreturn)
    );
}

/// Same as `jump_to_ring3`, but for a process with its own address space:
/// switches CR3 first, so `entry`/`user_stack_top` are resolved against
/// the *new* page tables, not whatever was active before. Used for a
/// process's very first entry into ring 3 (initial exec, not a return
/// from a later syscall/interrupt).
///
/// # Safety
/// `cr3` must point to a valid, fully set-up P4 table (see
/// `mem::paging::PageMapper::new_process_space`) with `entry` and
/// `user_stack_top` both already mapped in it.
#[cfg(feature = "paging")]
pub unsafe fn jump_to_ring3_in(entry_addr: u64, user_stack_top: u64, cr3: u64) -> ! {
    crate::mem::paging::write_cr3(cr3);
    let user_cs = (USER_CODE_SELECTOR | 3) as u64;
    let user_ss = (USER_DATA_SELECTOR | 3) as u64;
    let rflags: u64 = 0x202;

    asm!(
        "push {ss}",
        "push {stack}",
        "push {flags}",
        "push {cs}",
        "push {entry}",
        "iretq",
        ss = in(reg) user_ss,
        stack = in(reg) user_stack_top,
        flags = in(reg) rflags,
        cs = in(reg) user_cs,
        entry = in(reg) entry_addr,
        options(noreturn)
    );
}
