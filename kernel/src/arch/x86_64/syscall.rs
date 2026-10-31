//! SYSCALL/SYSRET support for x86_64.
//!
//! The `syscall` instruction provides fast user→kernel transitions. It uses
//! MSRs to configure entry points and segment selectors, bypassing the IDT.
//!
//! Register convention (matches Linux):
//! - RAX: syscall number
//! - RDI, RSI, RDX, R10, R8, R9: arguments 1-6
//! - RAX: return value (negative = error)
//! - RCX, R11: clobbered by syscall (hold RIP and RFLAGS)

use super::gdt::KERNEL_CODE_SELECTOR;
use core::arch::{asm, naked_asm};

// Model-Specific Registers for SYSCALL/SYSRET
const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

// EFER bits
const EFER_SCE: u64 = 1 << 0; // System Call Extensions enable

// RFLAGS bits to clear on syscall entry
const RFLAGS_IF: u64 = 1 << 9; // Interrupt Flag
const RFLAGS_TF: u64 = 1 << 8; // Trap Flag
const RFLAGS_DF: u64 = 1 << 10; // Direction Flag

/// Read a Model-Specific Register.
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack, preserves_flags)
    );
    ((high as u64) << 32) | (low as u64)
}

/// Write a Model-Specific Register.
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack, preserves_flags)
    );
}

/// The kernel-mode stack `syscall_entry` switches to. Read fresh on every
/// `syscall` instruction, so whichever task is actually running gets its
/// *own* stack rather than everyone sharing one — the scheduler calls
/// [`set_kernel_stack_top`] on every switch to keep this current,
/// mirroring `gdt::set_kernel_stack`'s TSS.RSP0 (same idea, two
/// mechanisms: TSS.RSP0 is consulted by the CPU on interrupts/exceptions,
/// this is consulted by our own `syscall_entry` code). Initialized in
/// [`init`] to `SYSCALL_STACK`'s top, so the boot-time `usermode-demo`
/// path (no scheduler, nobody else ever calls `set_kernel_stack_top`)
/// keeps working exactly as before.
#[no_mangle]
static mut CURRENT_KERNEL_STACK_TOP: u64 = 0;

/// Point `syscall_entry` at a different kernel stack for the next
/// `syscall` it handles. See [`CURRENT_KERNEL_STACK_TOP`].
pub fn set_kernel_stack_top(top: u64) {
    unsafe {
        CURRENT_KERNEL_STACK_TOP = top;
    }
}

/// Initialize SYSCALL/SYSRET support.
///
/// # Safety
/// Must be called after GDT is set up, from kernel mode.
pub unsafe fn init() {
    CURRENT_KERNEL_STACK_TOP =
        core::ptr::addr_of!(SYSCALL_STACK.0) as u64 + SYSCALL_STACK_SIZE as u64;

    // Enable SCE (System Call Extensions) in EFER
    let efer = rdmsr(IA32_EFER);
    wrmsr(IA32_EFER, efer | EFER_SCE);

    // STAR: segment selectors for SYSCALL and SYSRET
    // Bits 47:32 = SYSCALL CS (SS = CS + 8)
    // Bits 63:48 = SYSRET base (CS = base + 16, SS = base + 8)
    //
    // With our GDT layout:
    //   0x08 = kernel code, 0x10 = kernel data
    //   0x18 = user data, 0x20 = user code
    //
    // SYSCALL: CS=0x08, SS=0x10 (kernel)
    // SYSRET:  CS=0x10+16=0x20, SS=0x10+8=0x18 (user, with RPL=3 added by CPU)
    let star = ((0x10_u64) << 48) | ((KERNEL_CODE_SELECTOR as u64) << 32);
    wrmsr(IA32_STAR, star);

    // LSTAR: RIP for syscall entry
    wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);

    // FMASK: RFLAGS bits to clear on syscall entry
    // Clear IF (disable interrupts), TF (no single-step), DF (clear direction)
    wrmsr(IA32_FMASK, RFLAGS_IF | RFLAGS_TF | RFLAGS_DF);
}

/// Syscall entry point. Called directly by the CPU on `syscall` instruction.
///
/// On entry:
/// - RCX = user RIP (return address)
/// - R11 = user RFLAGS
/// - RSP = user stack (unchanged by syscall!)
/// - Interrupts disabled (FMASK cleared IF)
///
/// We must:
/// 1. Switch to kernel stack
/// 2. Save user state
/// 3. Dispatch syscall
/// 4. Restore user state
/// 5. Execute sysret
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        // Stash user RSP directly in memory instead of a register: R10
        // holds arg4 per the syscall convention, and any register we'd
        // reach for here is either carrying an argument or is
        // callee-saved (and thus needed intact for `push rbx`/etc.
        // below). A dedicated scratch slot sidesteps the problem
        // entirely — this used to clobber R10 into RSP and just pass 0
        // for arg4-6, which was fine while nothing needed them.
        "mov [{user_rsp}], rsp",

        // Switch to whichever task is actually running's own kernel
        // stack (see `CURRENT_KERNEL_STACK_TOP`'s doc comment) — not a
        // fixed symbol, so two different processes handling syscalls
        // don't corrupt each other's saved state.
        "mov rsp, [{stack_top}]",

        // Build a syscall frame on the kernel stack
        // Save user state
        "push qword ptr [{user_rsp}]", // user RSP
        "push rcx",     // user RIP (saved by syscall instruction)
        "push r11",     // user RFLAGS (saved by syscall instruction)

        // Save callee-saved registers
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Set up arguments for dispatch(number, arg1, arg2, arg3, arg4, arg5, arg6)
        // On syscall entry: RAX=number, RDI=arg1, RSI=arg2, RDX=arg3, R10=arg4, R8=arg5, R9=arg6
        // C ABI wants:       RDI=number, RSI=arg1, RDX=arg2, RCX=arg3, R8=arg4, R9=arg5, [rsp]=arg6
        //
        // R10 was never touched above (unlike the old "mov r10, rsp"),
        // so arg4 is still sitting there. Each line below reads a
        // register before overwriting it, so this is safe in this order.
        "push r9",      // arg6 -> stack (7th C-ABI arg)

        // The full 10-qword frame [arg6,r15,r14,r13,r12,rbp,rbx,r11,rcx,
        // user_rsp] now sits at the very top of this task's kernel stack
        // (see `SyscallFrame` and `current_frame`). Nothing below here
        // touches it until the matching pops in `syscall_return_to_user`.

        "mov r9, r8",   // arg5 -> r9
        "mov r8, r10",  // arg4 -> r8
        "mov rcx, rdx", // arg3
        "mov rdx, rsi", // arg2
        "mov rsi, rdi", // arg1
        "mov rdi, rax", // syscall number

        // Re-enable interrupts now that we're on kernel stack
        "sti",

        // Call the Rust syscall dispatcher
        "call {syscall_dispatch}",

        // RAX already contains return value from syscall_dispatch.
        // `syscall_return_to_user` (shared with `fork_child_entry`, see
        // its doc comment) does the rest: disable interrupts, pop the
        // frame, sysretq.
        "jmp {tail}",

        user_rsp = sym USER_RSP_SCRATCH,
        stack_top = sym CURRENT_KERNEL_STACK_TOP,
        syscall_dispatch = sym crate::syscall::dispatch,
        tail = sym syscall_return_to_user,
    );
}

/// Shared tail of every return to ring 3: disable interrupts, pop the
/// 10-qword frame at the top of the current kernel stack (arg6 discarded,
/// then the same layout `SyscallFrame` describes), `sysretq`.
///
/// Two callers land here: `syscall_entry` falls straight through after
/// `dispatch()` returns normally (RAX already holds its return value),
/// and [`fork_child_entry`] jumps here as a *forked child's* very first
/// instruction, with RSP planted by `sched`'s fork support pointing at a
/// copy of the parent's frame instead of a frame `dispatch()` actually
/// built — indistinguishable to this code either way, which is the
/// point: a forked child resumes exactly like an ordinary syscall return,
/// just with someone else's stack under it and RAX forced to 0.
#[unsafe(naked)]
pub extern "C" fn syscall_return_to_user() {
    naked_asm!(
        "cli",
        "add rsp, 8", // discard arg6
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11", // user RFLAGS
        "pop rcx", // user RIP
        "pop rsp", // user RSP
        "sysretq",
    );
}

/// A forked child's planted first instruction (see `sched`'s fork
/// support): zero RAX — `fork()` returns 0 in the child — then fall into
/// the same frame-restoring tail an ordinary syscall return uses. RSP at
/// entry already points at the copied `SyscallFrame`; `sched` is
/// responsible for arranging that before this ever runs.
#[unsafe(naked)]
pub extern "C" fn fork_child_entry() {
    naked_asm!(
        "xor eax, eax",
        "jmp {tail}",
        tail = sym syscall_return_to_user,
    );
}

/// The syscall-entry register frame `syscall_entry` builds on the kernel
/// stack before calling `dispatch()`. Field order matches push order
/// exactly (this is `#[repr(C)]` precisely so
/// the two stay in sync); `rflags`/`rip` are named for what they hold
/// (R11/RCX at syscall entry), not the registers themselves.
#[repr(C)]
pub struct SyscallFrame {
    pub arg6: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rflags: u64,
    pub rip: u64,
    pub user_rsp: u64,
}

/// The calling task's in-flight syscall register frame. `fork()` reads it
/// to seed a child's resume state; `execve()` overwrites `rip`/`user_rsp`/
/// `rflags` in place to redirect where this syscall returns to.
///
/// Derived from the running task's own kernel stack rather than recorded
/// anywhere: `syscall_entry` always starts from the top of that stack, so
/// the frame is always the top `size_of::<SyscallFrame>()` bytes of it (and
/// `sched`'s fork support plants a child's copy at the same place). The
/// scheduler keeps [`CURRENT_KERNEL_STACK_TOP`] pointing at the running
/// task's stack, so this is right however long the handler has run and
/// whatever other tasks did in the meantime.
///
/// This replaced a global that `syscall_entry` set on every syscall. Any
/// preemption before the handler read it let another process's syscall
/// overwrite it, so `execve` (which copies argv in first, plenty of time
/// for a tick) redirected *that* process to the new image and returned
/// into the unmapped old one itself. It crashed about one boot in two.
///
/// # Safety
/// Only valid while called from within a syscall handler (i.e. from code
/// reachable from `dispatch()`).
pub unsafe fn current_frame() -> *mut SyscallFrame {
    (CURRENT_KERNEL_STACK_TOP as usize - core::mem::size_of::<SyscallFrame>()) as *mut SyscallFrame
}

/// One-slot scratch space for stashing the user's RSP across the stack
/// switch in `syscall_entry`, freeing R10 to keep carrying arg4.
///
/// It's written and then immediately consumed a few instructions later,
/// entirely before `sti` re-enables interrupts — so on a single core
/// nothing can re-enter `syscall_entry` while it's live. A second core
/// running this same path concurrently would race on it; fine for now
/// (basicos is single-core), but this needs to become per-CPU the day
/// that changes.
#[no_mangle]
static mut USER_RSP_SCRATCH: u64 = 0;

/// Dedicated stack for syscall handling.
const SYSCALL_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
#[allow(dead_code)]
struct SyscallStack([u8; SYSCALL_STACK_SIZE]);

#[no_mangle]
static mut SYSCALL_STACK: SyscallStack = SyscallStack([0; SYSCALL_STACK_SIZE]);
