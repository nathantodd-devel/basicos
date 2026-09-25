//! Process/task abstraction + a preemptive round-robin scheduler.
//!
//! Two kinds of task share one run queue: **ring-0 kernel threads**
//! (no address space of their own — everything still shares the boot-time
//! identity map) and, since this module grew real per-process address
//! spaces, **ring-3 processes** (their own page tables, their own kernel
//! stack, real `fork`/`execve`/`wait`). The module docs below describe
//! the plain kernel-thread case first (unchanged from before), then the
//! process-specific machinery that layers on top of it.
//!
//! # The context switch
//!
//! `switch()` is the classic minimal trick (same one xv6, and most
//! teaching kernels, use): a context switch is implemented as an
//! ordinary function call, so the compiler has already arranged for
//! caller-saved registers to not matter across it. All `switch()` saves
//! by hand is what a normal `call`/`ret` wouldn't cover on its own — the
//! callee-saved registers (RBX, RBP, R12-R15) and RSP itself:
//!
//! 1. Push the 6 callee-saved registers onto the current stack.
//! 2. Save the resulting RSP into the outgoing task's slot.
//! 3. Load RSP from the incoming task's slot.
//! 4. Pop 6 callee-saved registers (now the incoming task's, from when
//!    *it* last called `switch()`) and `ret`.
//!
//! That `ret` is what actually resumes the other task: it pops
//! whatever return address was sitting on top of *its* saved stack,
//! which is either a previous call site inside `switch()`, or — for a
//! brand new task — an address we planted by hand in `spawn()` to make
//! the very first `ret` land on the task's entry function instead.
//!
//! # Preemption
//!
//! The timer IRQ (`arch::x86_64::interrupts::timer_interrupt_handler`)
//! calls [`yield_now`] on every tick, after sending EOI (this order
//! matters — see the comment there). Because `switch()` is just a
//! function call, calling it from inside an `extern "x86-interrupt"`
//! handler is completely ordinary: everything the CPU pushed for the
//! interrupt itself stays untouched, further down the *same* kernel
//! stack, below wherever `switch()` parks this task. When the task is
//! scheduled again, `switch()`'s `ret` resumes right after the
//! `yield_now()` call inside the (long-since-technically-"returned",
//! but suspended) handler invocation, which then runs its normal
//! `iretq` epilogue and the task's interrupted code continues exactly
//! where the timer caught it. IDT gates are interrupt gates (IF cleared
//! on entry — see `idt.rs`), and nothing here re-enables interrupts
//! mid-switch, so a second timer tick can't land in the middle of one.
//!
//! # Ring-3 processes: per-task kernel stack + CR3
//!
//! A ring-3 program used to be a permanent, one-way `gdt::jump_to_ring3`
//! call, never a scheduler task — the old version of this comment block
//! explained why: a timer tick preempting ring-3 code lands on
//! `TSS.RSP0`, and with a *single* shared `TSS.RSP0`/syscall stack for
//! every task, two ring-3 programs (or even one, preempted mid-syscall)
//! would corrupt each other's saved state.
//!
//! That's fixed now the straightforward way: every task — kernel thread
//! or process — already has its own 16 KiB stack (`TASK_STACKS`, used by
//! `switch()` itself). `run()` now also points `TSS.RSP0`
//! (`gdt::set_kernel_stack`) and `syscall_entry`'s own target
//! (`arch::x86_64::syscall::set_kernel_stack_top`) at that *same* stack
//! on every switch-in, for every task. A process's own syscalls and any
//! interrupt that catches it in ring 3 both land on its own stack, not a
//! shared one — the exact bug the old design couldn't avoid.
//!
//! The other half is CR3: `run()` also reloads CR3 on every switch-in —
//! the boot address space for a kernel thread, a process's own
//! `ProcessInfo::cr3` for a process. Nothing else touches CR3 between
//! switches (not `switch()`, not `yield_now()`), so it's always correct
//! by the time anything user-visible runs, even though it can sit briefly
//! stale (pointing at whatever the *previous* task used) while `run()`'s
//! own bookkeeping executes — harmless, since kernel code and data are
//! identically mapped in every address space by construction (see
//! `mem::paging`'s module docs on the process zone).
//!
//! # fork()
//!
//! A forked child needs to resume as if it were the *same syscall*
//! returning a second time, with its own copy of the parent's memory and
//! registers. It's spliced in below the normal "first switch into a new
//! task" trampoline: instead of landing on an ordinary
//! `extern "C" fn() -> !`, a forked child's planted stack has
//! `arch::x86_64::syscall::fork_child_entry` as its `switch()`-ret
//! target, with a copy of the parent's in-flight `SyscallFrame` sitting
//! right below it — see that function's doc comment for the rest.

use crate::arch::gdt;
use crate::arch::interrupts;
#[cfg(feature = "elf-loader")]
use crate::arch::x86_64::syscall as arch_syscall;
#[cfg(feature = "elf-loader")]
use crate::mem::paging;
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "elf-loader")]
use core::sync::atomic::AtomicU64;

mod demo;

/// Fixed task-table size — no dynamic task creation beyond this.
pub const MAX_TASKS: usize = 8;

/// Kernel stack given to each task. Plenty for the kinds of things a
/// ring-0 thread does here (no deep recursion, no large stack arrays) —
/// and, now, also what a process's own `TSS.RSP0`/syscall stack points
/// at while it's the one running (see the module docs).
const TASK_STACK_SIZE: usize = 16 * 1024;

const NONE_RUNNING: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Unused,
    Ready,
    Running,
    /// A kernel thread that ran off the end (nothing here does this
    /// today, but `exit_current` supports it) — never scheduled again,
    /// and unlike `Zombie`, nobody's waiting to reap it.
    Done,
    /// A process that called `exit()`: its address space is already
    /// freed (see `exit_process`), but the slot itself — pid, ppid, exit
    /// status — sticks around until `wait`/`waitpid` reaps it.
    Zombie,
}

#[derive(Clone, Copy)]
struct TaskSlot {
    state: TaskState,
    /// Saved stack pointer. Only meaningful while `state != Running`;
    /// while running, the task's real RSP is just... RSP.
    rsp: u64,
    name: &'static str,
}

impl TaskSlot {
    const fn empty() -> Self {
        Self {
            state: TaskState::Unused,
            rsp: 0,
            name: "",
        }
    }
}

#[repr(align(16))]
struct TaskStack([u8; TASK_STACK_SIZE]);

// Single-core, and every access below is either made with interrupts
// disabled or is the switch()/spawn() bookkeeping that only ever runs
// on behalf of "whichever task currently holds the CPU" — see the
// module docs. No SMP, so no atomics/locks needed for the table itself.
static mut TASKS: [TaskSlot; MAX_TASKS] = [TaskSlot::empty(); MAX_TASKS];
static mut TASK_STACKS: [TaskStack; MAX_TASKS] =
    [const { TaskStack([0; TASK_STACK_SIZE]) }; MAX_TASKS];

/// The scheduler loop's own saved RSP — where a task's `switch()` call
/// lands to hand control back to [`run`].
static mut SCHED_RSP: u64 = 0;

/// Index of the currently-running task in `TASKS`, or [`NONE_RUNNING`]
/// while the scheduler loop itself has the CPU (picking, or idling).
static CURRENT: AtomicUsize = AtomicUsize::new(NONE_RUNNING);

/// Round-robin search position, so `pick_next` doesn't always start
/// from 0 and starve later slots.
static NEXT_HINT: AtomicUsize = AtomicUsize::new(0);

fn task_stack_top(idx: usize) -> u64 {
    unsafe { core::ptr::addr_of_mut!(TASK_STACKS[idx].0) as u64 + TASK_STACK_SIZE as u64 }
}

/// Minimal context switch — see the module docs for exactly what this
/// does and why it's safe to call from an interrupt handler.
///
/// RFLAGS is saved and restored along with the callee-saved registers,
/// so **every context resumes with the interrupt-enable state it had
/// when it switched out** — a task that yielded from a syscall (IF=1)
/// comes back with IF=1, one suspended inside a timer handler (IF=0,
/// since interrupt gates clear it) comes back with IF=0 and re-enables
/// on its own `iretq`, and the scheduler loop (which runs with IF=0, see
/// [`run`]) comes back with IF=0.
///
/// This replaced an unconditional `sti` on every switch, which was
/// wrong in a way that's easy to miss: it turned interrupts on while the
/// CPU was still in scheduler context (or mid-way through a switch to
/// it), where [`CURRENT`] doesn't describe what's actually running. A
/// timer tick landing there made `yield_now` save the *scheduler's* RSP
/// into a task's slot, and the next switch into that task popped garbage
/// off the boot stack. It only showed up once a single task sat in a
/// tight `yield` loop (init, as the only task), because that's when the
/// scheduler is switching constantly and ticks reliably land in the
/// window.
#[unsafe(naked)]
unsafe extern "C" fn switch(old_rsp: *mut u64, new_rsp: u64) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "pushfq",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "popfq",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}

/// RFLAGS a brand-new task starts with: IF set (a kernel thread has to
/// be preemptible from its first instruction; a process gets IF from its
/// `iretq` frame anyway), plus the always-1 reserved bit 1.
const INITIAL_RFLAGS: u64 = 0x202;

/// Plant a fake `switch()` frame on task `idx`'s stack so the *first*
/// switch into it loads `INITIAL_RFLAGS`, pops six zeroed callee-saved
/// registers, and then
/// `ret`s straight into `entry`, as if `entry` had just been `call`ed.
/// Shared by every "brand new task" path (`spawn`, `spawn_process`, and
/// fork's child) — they differ only in what `entry` is and what (if
/// anything) sits below it on the stack.
///
/// # Safety
/// `idx` must be a task slot nobody else is using.
unsafe fn plant_initial_frame(idx: usize, entry: u64, extra: &[u64]) -> u64 {
    let mut sp = task_stack_top(idx) as *mut u64;
    for &word in extra.iter().rev() {
        sp = sp.sub(1);
        *sp = word;
    }
    // The padding slot only exists to satisfy the SysV ABI's "just
    // entered a function via `call`" alignment for a plain
    // `extern "C" fn() -> !` entry (`spawn`/`ring3_task_entry`'s case,
    // `extra` empty) — it must *not* be inserted when `extra` is
    // present (`fork_child_entry`'s case): that data is a raw register
    // frame with its own fixed layout that `syscall_return_to_user`
    // reads starting at the very next pop after `entry`'s `ret`, and an
    // extra padding qword between them would shift every field off by
    // one slot.
    if extra.is_empty() {
        sp = sp.sub(1);
        *sp = 0; // padding, for alignment
    }
    sp = sp.sub(1);
    *sp = entry; // switch()'s `ret` target
    for _ in 0..6 {
        sp = sp.sub(1);
        *sp = 0; // rbp, rbx, r12, r13, r14, r15 (in that pop order)
    }
    sp = sp.sub(1);
    *sp = INITIAL_RFLAGS; // popped first by switch()'s `popfq`
    sp as u64
}

/// Create a new task from `entry`, which should run indefinitely (or
/// call [`exit_current`]) rather than returning — there's nowhere for
/// it to return *to*.
///
/// Returns `None` if the task table is full.
pub fn spawn(name: &'static str, entry: extern "C" fn() -> !) -> Option<usize> {
    unsafe {
        let idx = (0..MAX_TASKS).find(|&i| TASKS[i].state == TaskState::Unused)?;
        let rsp = plant_initial_frame(idx, entry as u64, &[]);
        TASKS[idx] = TaskSlot {
            state: TaskState::Ready,
            rsp,
            name,
        };
        Some(idx)
    }
}

fn pick_next() -> Option<usize> {
    let start = NEXT_HINT.load(Ordering::Relaxed);
    for offset in 0..MAX_TASKS {
        let idx = (start + offset) % MAX_TASKS;
        // Safety: read-only glance at `state`; the only writes happen
        // with interrupts disabled or from the task that owns the slot.
        if unsafe { TASKS[idx].state } == TaskState::Ready {
            NEXT_HINT.store((idx + 1) % MAX_TASKS, Ordering::Relaxed);
            return Some(idx);
        }
    }
    None
}

/// The scheduler loop. Call once, after spawning whatever tasks should
/// start ready; never returns.
///
/// Runs with interrupts **disabled** for everything except the idle
/// `sti; hlt`: `CURRENT` and the task table are only meaningful to a
/// timer tick's `yield_now` while a task really is on the CPU, and the
/// stretch between "CURRENT = idx" and the switch (and back) is exactly
/// where that stops being true. Each task's own interrupt state comes
/// and goes with it via [`switch`]'s RFLAGS save/restore.
pub fn run() -> ! {
    interrupts::disable();
    loop {
        match pick_next() {
            Some(idx) => unsafe {
                TASKS[idx].state = TaskState::Running;
                CURRENT.store(idx, Ordering::Relaxed);

                let stack_top = task_stack_top(idx);
                gdt::set_kernel_stack(stack_top);
                #[cfg(feature = "elf-loader")]
                arch_syscall::set_kernel_stack_top(stack_top);
                #[cfg(feature = "elf-loader")]
                paging::write_cr3(proc_cr3_or_boot(idx));

                switch(core::ptr::addr_of_mut!(SCHED_RSP), TASKS[idx].rsp);
                // Resumes here once `idx` yields, is preempted back, or
                // exits — in every case it has already updated its own
                // `state` (Ready, Done, or Zombie) before switching back
                // to us.
                CURRENT.store(NONE_RUNNING, Ordering::Relaxed);
            },
            None => {
                // Nothing ready. Halt until the next interrupt (the
                // timer, if nothing else) and look again. `sti` and
                // `hlt` in one asm block on purpose: `sti`'s
                // one-instruction shadow means an interrupt can't slip
                // in between them and be lost to a `hlt` that then
                // sleeps through it. CURRENT is NONE_RUNNING here, so a
                // tick's `yield_now` returns immediately; we turn
                // interrupts back off before touching anything.
                unsafe { core::arch::asm!("sti", "hlt") };
                interrupts::disable();
            }
        }
    }
}

#[cfg(feature = "elf-loader")]
fn proc_cr3_or_boot(idx: usize) -> u64 {
    let pid = unsafe { PROC_INFO[idx].pid };
    if pid == 0 {
        paging::boot_cr3()
    } else {
        unsafe { PROC_INFO[idx].cr3 }
    }
}

/// Voluntarily give up the CPU. Also what the timer IRQ calls to
/// preempt whatever's currently running (see the module docs). A no-op
/// if called while the scheduler itself has the CPU (nothing to yield
/// from) — safe to call unconditionally from an interrupt handler that
/// might fire before `run()` ever starts.
pub fn yield_now() {
    let current = CURRENT.load(Ordering::Relaxed);
    if current == NONE_RUNNING {
        return;
    }
    unsafe {
        if TASKS[current].state == TaskState::Running {
            TASKS[current].state = TaskState::Ready;
        }
        switch(core::ptr::addr_of_mut!(TASKS[current].rsp), SCHED_RSP);
        // Resumes here once `run()` schedules this task again.
    }
}

/// End the current task for good and switch back to the scheduler. The
/// slot is marked `Done` and never picked again.
///
/// # Panics
/// If called from outside any task (nothing to exit).
pub fn exit_current() -> ! {
    let current = CURRENT.load(Ordering::Relaxed);
    assert_ne!(current, NONE_RUNNING, "exit_current() outside a task");
    unsafe {
        TASKS[current].state = TaskState::Done;
        switch(core::ptr::addr_of_mut!(TASKS[current].rsp), SCHED_RSP);
    }
    unreachable!("a Done task is never scheduled again");
}

/// Snapshot the task table for inspection (e.g. a `ps` shell command).
/// `f` is called once per non-`Unused` slot with `(name, state)`.
pub fn for_each_task(mut f: impl FnMut(&'static str, TaskState)) {
    let was_enabled = interrupts::disable();
    for i in 0..MAX_TASKS {
        // Safety: interrupts are off, so nothing else touches `TASKS`
        // for the duration of this snapshot.
        let (state, name) = unsafe { (TASKS[i].state, TASKS[i].name) };
        if state != TaskState::Unused {
            f(name, state);
        }
    }
    interrupts::restore(was_enabled);
}

pub use demo::{demo_task_a, demo_task_b};

// ============================================================================
// Processes: fork/exec/wait, mmap/brk — everything above this line is the
// original ring-0-only scheduler, unchanged. Everything below is new.
// ============================================================================

#[cfg(feature = "elf-loader")]
mod process {
    use super::*;
    use crate::mem::paging::{
        flags, PageMapper, VirtualAddress, PAGE_SIZE, PROC_HEAP_START, PROC_MMAP_START,
        PROC_MMAP_SIZE, PROC_STACK_START, PROC_STACK_SIZE,
    };
    use crate::pr_info;
    use alloc::vec::Vec;

    /// Per-process state, parallel to `TASKS` (same index). `pid == 0`
    /// means "this slot is a plain kernel thread, not a process" — real
    /// pids start at 1, so this doubles as the "is this a process" test
    /// without a separate field.
    #[derive(Clone, Copy)]
    pub struct ProcessInfo {
        pub pid: u64,
        pub ppid: u64,
        pub cr3: u64,
        /// Only meaningful before a brand-new (non-forked) process's
        /// first run — `ring3_task_entry` reads these once and never
        /// again.
        pub entry_point: u64,
        pub user_stack_top: u64,
        /// Current `brk()` break — grows up from `PROC_HEAP_START`.
        pub brk: u64,
        /// Current `mmap()` bump pointer — grows up from
        /// `PROC_MMAP_START`. No reuse on `munmap` (see `sys_munmap`),
        /// matching this kernel's other "good enough for now" allocators
        /// (the ramdisk's non-coalescing allocator is the same idea).
        pub mmap_next: u64,
        pub exit_status: i32,
    }

    impl ProcessInfo {
        const fn empty() -> Self {
            Self {
                pid: 0,
                ppid: 0,
                cr3: 0,
                entry_point: 0,
                user_stack_top: 0,
                brk: 0,
                mmap_next: 0,
                exit_status: 0,
            }
        }
    }

    pub static mut PROC_INFO: [ProcessInfo; MAX_TASKS] = [ProcessInfo::empty(); MAX_TASKS];
    static NEXT_PID: AtomicU64 = AtomicU64::new(1);

    /// How many pages of the (1 MiB, lazily-backed) stack region to map
    /// eagerly at process-creation time. There's no demand paging here —
    /// a stack that grows past this faults (unhandled `#PF`, currently a
    /// panic or halt depending on `bad-interrupt-panic`) — but 32 KiB is
    /// comfortable headroom for the kind of small test programs this
    /// kernel actually runs.
    const INITIAL_STACK_PAGES: usize = 8;

    /// A brand-new (never-yet-run) process's `switch()`-ret target:
    /// there's no in-flight syscall frame to resume into, just a plain
    /// jump to ring 3 at whatever `spawn_process`/`exec_into` set up —
    /// looked up via `CURRENT` since `switch()`'s `ret` can't pass
    /// arguments.
    extern "C" fn ring3_task_entry() -> ! {
        let idx = CURRENT.load(Ordering::Relaxed);
        let (entry, stack, cr3) = unsafe {
            let p = &PROC_INFO[idx];
            (p.entry_point, p.user_stack_top, p.cr3)
        };
        unsafe { gdt::jump_to_ring3_in(entry, stack, cr3) }
    }

    /// Map `INITIAL_STACK_PAGES` at the top of the process stack region
    /// and lay out a SysV-ish `argc`/`argv`/`envp` (envp always empty —
    /// no environment variables here) on it, returning the resulting
    /// user RSP. `argv[0]` onward are copied from `argv`.
    ///
    /// Writes through `space`'s own page-table walk (`translate`), not a
    /// raw virtual pointer, so this works correctly no matter which
    /// address space is currently loaded in CR3 — in particular,
    /// `spawn_process` calls this *before* the new process has ever been
    /// scheduled, when CR3 is whatever the caller (e.g. the shell) was
    /// already using.
    unsafe fn setup_stack(space: &mut PageMapper, argv: &[&str]) -> u64 {
        let stack_top = (PROC_STACK_START + PROC_STACK_SIZE) as u64;
        let region_start = stack_top - (INITIAL_STACK_PAGES * PAGE_SIZE) as u64;
        for i in 0..INITIAL_STACK_PAGES {
            space
                .alloc_and_map(region_start + (i * PAGE_SIZE) as u64, flags::USER_PAGE)
                .expect("out of memory mapping initial process stack");
        }

        let write_byte = |virt: u64, b: u8| {
            let phys = space.translate(VirtualAddress::new(virt)).expect("just mapped");
            *(phys as *mut u8) = b;
        };
        // Safe: an 8-byte-aligned write of 8 bytes never straddles a
        // 4 KiB page boundary (4096 is a multiple of 8), so unlike
        // string bytes (arbitrary alignment, written one at a time
        // above), a single-page `translate` is enough for the whole
        // write — every call site below only ever writes at 8-aligned
        // `sp` values.
        let write_u64 = |virt: u64, v: u64| {
            let phys = space.translate(VirtualAddress::new(virt)).expect("just mapped");
            *(phys as *mut u64) = v;
        };

        let mut sp = stack_top;

        // Copy each argv string, highest address first, remembering
        // where each landed.
        let mut argv_ptrs = Vec::with_capacity(argv.len());
        for s in argv.iter().rev() {
            let bytes = s.as_bytes();
            sp -= (bytes.len() + 1) as u64; // +1 for the NUL
            for (i, &b) in bytes.iter().enumerate() {
                write_byte(sp + i as u64, b);
            }
            write_byte(sp + bytes.len() as u64, 0);
            argv_ptrs.push(sp);
        }
        argv_ptrs.reverse();

        sp &= !0xF; // 16-byte align before the pointer arrays

        // envp: just a NULL terminator — no environment variables yet.
        sp -= 8;
        write_u64(sp, 0);

        // argv[]: NULL-terminated.
        sp -= 8;
        write_u64(sp, 0);
        for &ptr in argv_ptrs.iter().rev() {
            sp -= 8;
            write_u64(sp, ptr);
        }

        // argc, at the very bottom — what RSP points to at entry.
        sp -= 8;
        write_u64(sp, argv.len() as u64);

        sp
    }

    /// Start a brand-new process by loading `path` as an ELF64 binary,
    /// giving it its own address space and a scheduler slot. Used by the
    /// shell's `run` command — the one place a process gets created from
    /// nothing rather than via `fork()`.
    ///
    /// Unlike the old `elf::run`, this returns immediately (the new
    /// process is just queued `Ready`) instead of jumping there directly
    /// — it's a real scheduler task now, so the caller (the shell) keeps
    /// running concurrently with it.
    pub fn spawn_process(name: &'static str, path: &str) -> Result<u64, crate::elf::RunError> {
        let (drive, rest) = crate::fs::resolve_path(path);
        let data = match crate::fs::with_drive(drive, |vol| vol.read_file_at(rest)) {
            Some(Ok(data)) => data,
            Some(Err(e)) => return Err(crate::elf::RunError::Fs(e)),
            None => return Err(crate::elf::RunError::NotFound),
        };

        let idx = unsafe { (0..MAX_TASKS).find(|&i| TASKS[i].state == TaskState::Unused) }
            .ok_or(crate::elf::RunError::NoTaskSlots)?;

        let mut space = PageMapper::new_process_space();
        let entry =
            unsafe { crate::elf::load_into(&mut space, &data) }.map_err(crate::elf::RunError::Elf)?;
        let stack_top = unsafe { setup_stack(&mut space, &[path]) };

        let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
        unsafe {
            PROC_INFO[idx] = ProcessInfo {
                pid,
                ppid: 0,
                cr3: space.cr3(),
                entry_point: entry,
                user_stack_top: stack_top,
                brk: PROC_HEAP_START as u64,
                mmap_next: PROC_MMAP_START as u64,
                exit_status: 0,
            };
            let rsp = plant_initial_frame(idx, ring3_task_entry as *const () as u64, &[]);
            TASKS[idx] = TaskSlot {
                state: TaskState::Ready,
                rsp,
                name,
            };
        }
        pr_info!("spawn_process: {} -> pid {}", path, pid);
        Ok(pid)
    }

    /// `fork()`: duplicate the calling process — its address space
    /// (a deep copy, not copy-on-write) and its in-flight syscall
    /// register state — into a new task, and return its pid. The
    /// syscall itself (`sys_fork`) is what actually returns `0` in the
    /// child and the new pid in the parent; this just does the
    /// duplication and queues the child `Ready`.
    ///
    /// Returns `None` if the task table is full.
    pub fn fork_current() -> Option<u64> {
        // Same reasoning as `exec_current`'s own doc comment: capture
        // this call's syscall frame *before* `fork_from`'s deep copy (a
        // lot of individually-brief-but-cumulatively-lengthy, fully
        // preemptible `frame::allocate()` calls) gives a timer tick the
        // chance to switch to some other task that makes a syscall of
        // its own — which would silently overwrite the single global
        // `current_frame()` reads from, with no reason to expect anyone
        // reading it late would notice it now points somewhere else.
        let frame = unsafe { &*arch_syscall::current_frame() };

        let parent_idx = CURRENT.load(Ordering::Relaxed);
        let child_idx = unsafe { (0..MAX_TASKS).find(|&i| TASKS[i].state == TaskState::Unused) }?;

        let parent_cr3 = unsafe { PROC_INFO[parent_idx].cr3 };
        let parent_space = PageMapper::for_address_space(parent_cr3);
        let child_space = PageMapper::fork_from(&parent_space);

        let child_pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

        unsafe {
            let parent = &PROC_INFO[parent_idx];
            PROC_INFO[child_idx] = ProcessInfo {
                pid: child_pid,
                ppid: parent.pid,
                cr3: child_space.cr3(),
                entry_point: 0,
                user_stack_top: 0,
                brk: parent.brk,
                mmap_next: parent.mmap_next,
                exit_status: 0,
            };

            // Plant the child's stack: `switch()` rets into
            // `fork_child_entry`, which finds this exact copy of the
            // parent's syscall frame sitting right where it lands — see
            // both functions' doc comments.
            let saved = [
                frame.arg6,
                frame.r15,
                frame.r14,
                frame.r13,
                frame.r12,
                frame.rbp,
                frame.rbx,
                frame.rflags,
                frame.rip,
                frame.user_rsp,
            ];
            let rsp = plant_initial_frame(
                child_idx,
                arch_syscall::fork_child_entry as *const () as u64,
                &saved,
            );
            TASKS[child_idx] = TaskSlot {
                state: TaskState::Ready,
                rsp,
                name: "fork",
            };
        }
        Some(child_pid)
    }

    /// `execve()`: tear down the calling process's *own* memory (code,
    /// heap, mmap'd pages, stack — everything in the process zone) and
    /// replace it with a freshly loaded ELF, redirecting the in-flight
    /// syscall to return into the new program instead of the old one.
    /// Same pid, same task slot, same parent — just a new image, exactly
    /// like the real thing.
    ///
    /// On success this doesn't "return" in any meaningful sense (the
    /// calling program is gone), but the Rust call itself does — the
    /// redirection happens by mutating the current `SyscallFrame` in
    /// place, which `syscall_return_to_user` picks up normally.
    pub fn exec_current(path: &str, argv: &[&str]) -> Result<(), crate::elf::RunError> {
        // Capture *this* call's own syscall frame right away, before
        // `load_into`/`setup_stack` below do a lot of preemptible work
        // (many individual `frame::allocate()` calls, each a brief
        // critical section but with interrupts back on in between).
        // `arch_syscall::current_frame()` reads a single global that
        // `syscall_entry` overwrites on *every* syscall, from *any*
        // task — if a timer tick preempted this task mid-`load_into` and
        // some other task made a syscall of its own before this one
        // resumed, a late read here would silently pick up that other
        // task's frame pointer instead of this call's own, and the
        // writes below would corrupt an unrelated process's return
        // state. Capturing the pointer now and reusing it, rather than
        // re-deriving it from the global later, sidesteps that
        // entirely — the pointee (a fixed spot on *this* task's own
        // kernel stack) doesn't move, only the global's current value is
        // unstable. `fork_current` already does the same thing for the
        // same reason.
        let frame = unsafe { arch_syscall::current_frame() };

        let (drive, rest) = crate::fs::resolve_path(path);
        let data = match crate::fs::with_drive(drive, |vol| vol.read_file_at(rest)) {
            Some(Ok(data)) => data,
            Some(Err(e)) => return Err(crate::elf::RunError::Fs(e)),
            None => return Err(crate::elf::RunError::NotFound),
        };

        let idx = CURRENT.load(Ordering::Relaxed);
        let cr3 = unsafe { PROC_INFO[idx].cr3 };
        let mut space = PageMapper::for_address_space(cr3);

        // Tear down the old image entirely before loading the new one —
        // `execve` fully replaces the process, no leftover mappings.
        unsafe { space.unmap_zone() };

        let entry =
            unsafe { crate::elf::load_into(&mut space, &data) }.map_err(crate::elf::RunError::Elf)?;
        let stack_top = unsafe { setup_stack(&mut space, argv) };

        unsafe {
            PROC_INFO[idx].brk = PROC_HEAP_START as u64;
            PROC_INFO[idx].mmap_next = PROC_MMAP_START as u64;

            (*frame).rip = entry;
            (*frame).user_rsp = stack_top;
            (*frame).rflags = 0x202;
        }
        Ok(())
    }

    /// `exit()`: free the process's own address space immediately (its
    /// pid/exit status live on in the — now `Zombie` — task slot, but
    /// there's no reason to hold onto its memory until someone reaps it)
    /// and switch back to the scheduler for good.
    ///
    /// # Panics
    /// If called from outside any task.
    pub fn exit_process(status: i32) -> ! {
        let idx = CURRENT.load(Ordering::Relaxed);
        assert_ne!(idx, NONE_RUNNING, "exit_process() outside a task");
        unsafe {
            let cr3 = PROC_INFO[idx].cr3;
            PageMapper::for_address_space(cr3).destroy();
            PROC_INFO[idx].exit_status = status;
            TASKS[idx].state = TaskState::Zombie;
            switch(core::ptr::addr_of_mut!(TASKS[idx].rsp), SCHED_RSP);
        }
        unreachable!("a Zombie task is never scheduled again");
    }

    /// `wait`/`waitpid`: block (via a plain yield loop — no dedicated
    /// "blocked" state, see the module docs' "good enough for now"
    /// theme) until a zombie child matching `target_pid` (or any child,
    /// if `None`) shows up, then reap it and return `(pid, exit_status)`.
    ///
    /// Returns `None` if the caller has no matching child at all (not
    /// even a running one) — there's nothing to ever wait for.
    pub fn wait_for_child(target_pid: Option<u64>) -> Option<(u64, i32)> {
        let caller_pid = current_pid();
        loop {
            let mut saw_any = false;
            let mut reaped = None;
            unsafe {
                for i in 0..MAX_TASKS {
                    if PROC_INFO[i].pid == 0 || PROC_INFO[i].ppid != caller_pid {
                        continue;
                    }
                    if let Some(want) = target_pid {
                        if PROC_INFO[i].pid != want {
                            continue;
                        }
                    }
                    saw_any = true;
                    if TASKS[i].state == TaskState::Zombie {
                        reaped = Some((PROC_INFO[i].pid, PROC_INFO[i].exit_status));
                        TASKS[i].state = TaskState::Unused;
                        PROC_INFO[i] = ProcessInfo::empty();
                        break;
                    }
                }
            }
            if let Some(result) = reaped {
                return Some(result);
            }
            if !saw_any {
                return None;
            }
            yield_now();
        }
    }

    /// The calling process's pid, or 0 if called from a plain kernel
    /// thread (shouldn't happen — only processes issue syscalls).
    pub fn current_pid() -> u64 {
        let idx = CURRENT.load(Ordering::Relaxed);
        if idx == NONE_RUNNING {
            return 0;
        }
        unsafe { PROC_INFO[idx].pid }
    }

    /// `brk(new_brk)`: grow or shrink the calling process's heap.
    /// `new_brk == 0` just queries the current break. Returns the
    /// resulting break either way (matching the real `brk()` ABI: it
    /// doesn't use errno, a failed grow just leaves the break where it
    /// was and returns that).
    pub fn proc_brk(new_brk: u64) -> u64 {
        let idx = CURRENT.load(Ordering::Relaxed);
        let (cr3, current) = unsafe { (PROC_INFO[idx].cr3, PROC_INFO[idx].brk) };
        if new_brk == 0 {
            return current;
        }
        let heap_end = (PROC_HEAP_START + crate::mem::paging::PROC_HEAP_SIZE) as u64;
        if new_brk < PROC_HEAP_START as u64 || new_brk > heap_end {
            return current;
        }

        let mut space = PageMapper::for_address_space(cr3);
        let page_align_up = |a: u64| (a + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        let old_top = page_align_up(current);
        let new_top = page_align_up(new_brk);

        unsafe {
            if new_top > old_top {
                let mut addr = old_top;
                while addr < new_top {
                    if space.alloc_and_map(addr, flags::USER_PAGE).is_err() {
                        return current; // out of memory; leave brk where it was
                    }
                    addr += PAGE_SIZE as u64;
                }
            } else if new_top < old_top {
                let mut addr = new_top;
                while addr < old_top {
                    let _ = space.unmap_and_free(addr);
                    addr += PAGE_SIZE as u64;
                }
            }
            PROC_INFO[idx].brk = new_brk;
        }
        new_brk
    }

    /// `mmap()`: anonymous pages only (no file-backed mappings — there's
    /// no page cache here to back them with). Bump-allocates from the
    /// calling process's mmap arena; `len` is rounded up to a whole
    /// number of pages. Returns the mapped address, or `None` if the
    /// arena is exhausted.
    pub fn proc_mmap(len: usize, writable: bool) -> Option<u64> {
        let idx = CURRENT.load(Ordering::Relaxed);
        let (cr3, next) = unsafe { (PROC_INFO[idx].cr3, PROC_INFO[idx].mmap_next) };
        let pages = len.div_ceil(PAGE_SIZE).max(1);
        let size = (pages * PAGE_SIZE) as u64;
        let arena_end = (PROC_MMAP_START + PROC_MMAP_SIZE) as u64;
        if next + size > arena_end {
            return None;
        }

        let mapflags = if writable {
            flags::USER_PAGE
        } else {
            flags::USER_READONLY
        };
        let mut space = PageMapper::for_address_space(cr3);
        unsafe {
            let mut addr = next;
            while addr < next + size {
                if space.alloc_and_map(addr, mapflags).is_err() {
                    return None;
                }
                addr += PAGE_SIZE as u64;
            }
            PROC_INFO[idx].mmap_next = next + size;
        }
        Some(next)
    }

    /// `munmap(addr, len)`: unmap and free pages the calling process
    /// itself mapped (via `mmap` or otherwise) inside its own mmap
    /// arena. No arena-space reuse (see `ProcessInfo::mmap_next`'s doc
    /// comment) — this only frees the physical frames.
    pub fn proc_munmap(addr: u64, len: usize) -> bool {
        if addr < PROC_MMAP_START as u64 || addr % PAGE_SIZE as u64 != 0 {
            return false;
        }
        let idx = CURRENT.load(Ordering::Relaxed);
        let cr3 = unsafe { PROC_INFO[idx].cr3 };
        let mut space = PageMapper::for_address_space(cr3);
        let pages = len.div_ceil(PAGE_SIZE).max(1);
        unsafe {
            for i in 0..pages {
                let _ = space.unmap_and_free(addr + (i * PAGE_SIZE) as u64);
            }
        }
        true
    }
}

#[cfg(feature = "elf-loader")]
pub use process::{
    current_pid, exec_current, exit_process, fork_current, proc_brk, proc_mmap, proc_munmap,
    spawn_process, wait_for_child, PROC_INFO,
};
