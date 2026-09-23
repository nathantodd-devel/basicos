# basicos TODO

## Logging System Enhancements

- [x] Reprogram PIT to 100Hz for accurate timestamps (currently using default ~18.2Hz)
- [ ] Add `pr_cont!` macro for continuation lines (like Linux's `KERN_CONT`)
- [ ] Implement `dmesg` command to dump ring buffer contents
- [ ] Add log level filtering at compile time (strip debug logs from release builds)
- [ ] Consider adding structured logging (key=value pairs)

## Memory Management

- [x] Implement physical frame allocator — bitmap-based, tracks 4 KiB frames up to 1 GiB
- [x] Implement heap allocator (`#[global_allocator]`) — free-list allocator; dynamically sized from `mem::frame` (claims most of physical RAM in one contiguous run, backing off geometrically until a claim succeeds) when `frame-alloc` is on, else the original fixed-size `.bss` region
- [x] Parse multiboot2 memory map to find usable RAM
- [x] Implement virtual memory manager / page table manipulation — 4-level paging with map/unmap/translate

## Interrupts & Hardware

- [x] Fix function pointer casts in `interrupts.rs` (add `as *const ()` intermediate cast)
- [x] Add more exception handlers (invalid opcode, FPU exceptions, etc.)
- [x] Implement APIC for multi-core support (replace legacy 8259 PIC)
- [x] Add HPET/APIC timer for high-resolution timing
- [x] Implement serial input (currently output-only)

## Userspace

- [x] Implement proper syscall interface — SYSCALL/SYSRET with open/read/write/lseek/close/getdents64/getpid/exit; fixed a real arg4-6 ABI bug (`syscall_entry` was clobbering R10 into RSP before reading arg4 out of it, so every syscall silently got 0 for args past the 3rd)
- [x] Add process/task abstraction — `sched::Task` (ring-0 kernel threads) plus real ring-3 **processes** with their own address space, pid/ppid, and per-task kernel/syscall stack (see the next few items — this used to be ring-0-only, see the git history for that caveat)
- [x] Implement scheduler (round-robin to start) — real preemption: an xv6-style minimal context switch (callee-saved regs + RSP only), triggered every PIT tick from the timer IRQ, not just cooperative yielding. Verified by two tasks that never call `yield_now()` themselves, whose output still interleaves. Found and fixed two real bugs along the way: RFLAGS.IF isn't part of the saved context (it's a single CPU-wide flag, not per-task), so a task preempted mid-interrupt-handler was leaving IF stuck at 0 for whatever ran next — fixed by an unconditional `sti` inside `switch()`; and `log.rs` wasn't holding a lock across a whole log line (just the ring-buffer write), so two tasks' output could interleave mid-word once real preemption existed to expose it — fixed by disabling interrupts for each `pr_*!`/`kprint!` call's full duration
- [x] Per-process address spaces — `mem::paging`'s process zone: `entrypoint.s`'s boot-time 2 MiB huge pages covering a fixed 16 MiB virtual slice (`PROC_ZONE_START`) get demoted to real 4 KiB page tables once, at boot (`demote_process_zone`), so each process (`PageMapper::new_process_space`/`fork_from`) can get its own private physical backing there — code, heap, mmap arena, and stack all live inside it — while everything else (kernel code/data/heap, page tables themselves) stays the single shared mapping every address space inherits by construction. Physical range reserved from `frame::allocate()` up front, which is what makes it sound for kernel code to `memcpy` into a freshly allocated frame via a raw physical pointer regardless of whose CR3 is currently loaded
- [x] Per-task kernel stack + `TSS.RSP0`/syscall-stack switching — closes the exact gap the scheduler used to have: `run()` now points `TSS.RSP0` (`gdt::set_kernel_stack`) and `syscall_entry`'s own stack target (`arch::x86_64::syscall::set_kernel_stack_top`) at whichever task is about to run, and reloads CR3 too, on *every* switch — a ring-3 process's own syscalls and any interrupt that catches it in ring 3 land on its own stack, not a shared one
- [x] ELF loader for userspace binaries — `elf::load_into`: parses ET_EXEC/EM_X86_64, copies PT_LOAD segments into a process's own address space (via its page-table walk, not a raw physical pointer — correct regardless of which CR3 is loaded). `elf::spawn`/`sched::spawn_process` load a binary and queue it as a real scheduler task (not a one-way trip anymore). Verified against two standalone binaries built completely outside the kernel: `testprog.s` (`make testprog` → `/hello.elf`) and `forktest.s` (`make forktest` → `/forktest.elf`, see below)
- [x] `fork()`/`execve()`/`wait4()` — real process duplication and image replacement, not stubs. `fork()` deep-copies the calling process's address space (`PageMapper::fork_from`, full copy, not copy-on-write) and splices a copy of its in-flight syscall register frame onto the child's own stack (`arch::x86_64::syscall::fork_child_entry`/`SyscallFrame`) so it resumes exactly where the parent called `fork()`, with RAX forced to 0. `execve()` tears down the caller's own memory (`PageMapper::unmap_zone`) and loads a fresh ELF into the same address space, redirecting the in-flight syscall's saved RIP/RSP instead of returning to the old program. `wait4()`/`sched::wait_for_child` blocks (a plain yield loop) until a zombie child shows up, reaps it, and returns its exit status. All four verified together end to end in QEMU via `forktest.elf`: fork → child execs into `hello.elf` (proving the teardown/reload is real, not a no-op) → child exits → parent reaps it via `wait4` — and `meminfo` shows byte-identical free frame counts before and after, confirming the address-space cleanup on `exit()`/`execve()` doesn't leak
- [x] `mmap()`/`munmap()`/`brk()` — anonymous-only (no page cache, so no file-backed mappings — `MAP_ANONYMOUS` or `EINVAL`); `mmap` bump-allocates from a per-process mmap arena, `brk` grows/shrinks a per-process heap region, both backed by real `alloc_and_map`/`unmap_and_free` calls into the calling process's own address space. Verified in `forktest.elf`: grows the heap, writes/reads a distinct value through the new page, maps an anonymous page, writes/reads a different value through *that*, unmaps it
- [x] Basic shell — `shell::shell_task`, runs as a scheduler task; `help`/`echo`/`clear`/`uptime`/`ps`/`meminfo`/`ls`/`cat`/`run`. Reads from serial if available, else keyboard. Enabling `scheduler` supersedes `usermode-demo`'s boot-time ring-3 jump (see `Cargo.toml`) — the two don't compose, and a shell is more useful as the default than a fixed demo. `run` now spawns a real background process (`elf::spawn`) instead of a one-way jump — the shell keeps running concurrently
- [ ] The shell doesn't reap its own spawned children — every `run` leaves a permanent zombie task slot once the process exits (nobody calls `wait()` on it). Harmless in practice (`ps` shows it as `Zombie` forever) but with `MAX_TASKS = 8`, running `run` more than a handful of times without anything reaping in between will exhaust the task table (`NoTaskSlots`). Needs either the shell backgrounding + reaping on the next prompt, or a `jobs`/foreground-wait command
- [ ] `fork()` is a full deep copy of the process's address space, not copy-on-write — correct, but means a large process's `fork()` is O(its memory), same as the mmap arena's "no reuse on `munmap`" simplification elsewhere. COW would need write-protecting the shared pages and a real page-fault handler integration; not done here
- [ ] No per-process file descriptor table — `syscall::OPEN_FILES` is still one global array shared by every process (predates this session's fork/exec work). A forked child currently doesn't get its own fd table the way real UNIX processes do; not exercised by anything yet since `forktest.elf` doesn't open files
- [ ] An unhandled fault in *any* ring-3 process halts the *entire kernel* — every CPU exception handler's fallback (`halt_loop()`) is an unconditional `cli;hlt` loop with no per-process recovery, so one process's bug (found via a user-added `userland/segv.c`, a deliberate null-pointer write) freezes the shell and every other task too, not just the faulting one. Pre-existing, not something this session's fork/exec/mmap work introduced or fixed — a real OS would kill just the offending process (`sched::exit_process` already exists and is exactly the mechanism a real fault handler would call) and keep the scheduler running

### Two real bugs found building `userland/` (a C toolchain against these syscalls)

Both were silent, not crashes — the kind that only show up once something exercises the exact pattern, which is exactly what happened building real (not hand-assembled) test programs against this syscall ABI for the first time:

- **`current_frame()` read too late.** `arch::x86_64::syscall::current_frame()` reads a single global `syscall_entry` overwrites on *every* syscall, from *any* task. `sched::fork_current`/`exec_current` both used to read it only after their own lengthy, fully-preemptible work (`fork_from`'s deep copy, `load_into`/`setup_stack`'s many `frame::allocate()` calls) — if a timer tick preempted mid-way and some *other* task made a syscall of its own before this one resumed, the late read silently picked up that other task's frame pointer, and `exec_current` would overwrite an unrelated process's saved RIP/RSP. Manifested as an intermittent page fault at a process-zone address, only when enough concurrent processes existed to make the race likely. Fixed by capturing the pointer at the very top of both functions, before any preemptible work — see both functions' doc comments and `current_frame()`'s own (now much longer) one.
- **GCC silently dropped a syscall argument.** `userland/syscalls.h`'s raw `__syscallN` wrappers are `static`, `noinline`, fully visible in their translation unit — and GCC's interprocedural constant propagation (`-O2`) clones a specialized copy per distinct constant syscall number (e.g. `__syscall1.constprop.0` for `close`'s always-3 `SYS_CLOSE`), and in the clone, "proved" the discarded read-write argument operand was dead and deleted the instruction that loaded it into its register entirely — so `sys_close(fd)` silently executed with whatever register value was already there from the *previous* syscall (observed: `close`'s fd argument arriving as the *previous* `write` call's byte count). `noinline` alone doesn't stop this — it's IPA cloning, a different optimization. Fixed with `__attribute__((noipa))`, which disables that cloning outright. See `syscalls.h`'s header comment and the comment above the `__syscallN` declarations for the full story.

## Userland C toolchain (`userland/`)

- [x] A real C compiler pointed at basicos's syscall ABI, not just hand-written assembly test programs — `userland/syscalls.h` (typed wrappers + raw `__syscallN` primitives, matching `syscall::nr`/`open_flags`/`whence`/`mmap_flags`/`tree_target`/`errno`), `userland/crt0.s` (the one piece that can't be C — a process's first instruction isn't call-shaped, see its header comment), `userland/user.ld` (shared linker script, same fixed `0x600000` load address as `testprog.ld`/`forktest.ld`), and `userland/Makefile` (`-ffreestanding -nostdlib -nostdinc -static -fno-pic -fno-pie -fno-stack-protector -fcf-protection=none -mno-red-zone -mgeneral-regs-only` — the last one is load-bearing, not style: the scheduler's context switch never saves/restores XMM/x87/MMX state, so ordinary C code reaching for SSE by default, as gcc does on x86_64, would silently corrupt it across a preemption). Wired into the top-level `Makefile` (`make userland`, and `make disk`/`make disk-reset` build + copy it automatically) and the test disk as `/chello.elf`, `/fileio.elf`, `/forkexec.elf`
- [x] Three example programs, each exercising syscalls the assembly test programs didn't: `hello.c` (write/exit, the C equivalent of `testprog.s`), `fileio.c` (open/write/read/close — a real round trip through the ramdisk), `forkexec.c` (brk/mmap/munmap, then fork/execve/wait4 — execve's into the *assembly*-built `/hello.elf` on purpose, to prove a C-compiled and a hand-assembled process interoperate through the same ABI). All three boot-verified in QEMU; building and running them is what surfaced the two bugs above

## VGA / Display

- [ ] Add color support to VGA driver (currently hardcoded light grey on black)
- [ ] Implement cursor blinking
- [ ] Consider framebuffer mode for graphics (VESA/GOP)

## Keyboard

- [x] Handle extended scancodes (arrows, function keys, etc.) — decoded as the ANSI/VT220 escape sequences a real terminal would send
- [x] Implement keyboard buffer/queue — ring buffer + read_byte/try_read_byte/has_data/read_line, mirrors `drivers::serial`'s input API
- [x] Add modifier key tracking (shift, ctrl, alt state) — `keyboard::modifiers()`; Ctrl+letter now produces real control codes (Ctrl+C = 0x03, etc.)

## Storage

- [x] ATA/AHCI driver for disk access — ATA PIO, primary bus/master drive only, polling (no AHCI/SATA)
- [x] Simple filesystem (FAT32 or custom) — FAT32 read/write, 8.3 names only, no subdirectory creation
- [x] initrd/ramdisk support — two flavors now: the read-only initrd (GRUB `module2` FAT32 image, `make initrd`), and a genuinely writable ramdisk (`fs::ramdisk` — a from-scratch FAT32 "mkfs" over a heap-allocated buffer, formatted fresh on every boot)
- [x] DOS-style drive letters — `A:` the ramdisk, `B:` the initrd, `C:` the main disk (`fs::resolve_path`/`fs::with_drive`), used by the syscall `open()` path and the shell's `ls`/`cat`/`write`/`run`; old no-prefix and `/initrd/...` paths still work, defaulting to `C:`/`B:` respectively

## Networking

Roughly bottom-up order — each step needs the one before it to test
against. `fs::BlockDevice` (device-agnostic driver behind a small trait)
is the template to follow for the NIC layer too.

- [ ] PCI bus enumeration (config space via ports 0xCF8/0xCFC, or MMIO
      ECAM) — needed just to find a NIC's BARs/IRQ line in the first place
- [ ] NIC driver — RTL8139 is the classic simple one to start with
      (QEMU's `-device rtl8139`); e1000 or virtio-net are the more
      "real" alternatives once the stack above it works. RX/TX
      descriptor rings need physically-contiguous DMA buffers —
      `mem::frame::allocate_contiguous` already exists for this
- [ ] `NetDevice` trait (mirrors `fs::BlockDevice`) so the protocol
      stack doesn't care which NIC driver is underneath — send/recv a
      raw Ethernet frame, report a MAC address and MTU
- [ ] Packet buffer management — a pool of fixed-size buffers (or just
      `alloc::vec::Vec<u8>` for a first pass) with headroom to prepend
      headers as a packet moves down the stack
- [ ] Ethernet layer — frame parsing/construction (dst/src MAC,
      EtherType), dispatch to ARP vs IPv4 by EtherType
- [ ] ARP — request/reply + a small IP-to-MAC cache; every IPv4 send
      needs this before it can address a frame
- [ ] IPv4 — header parse/build, checksum, a routing table (at minimum
      "local subnet vs. default gateway"); fine to just drop anything
      that would need fragmentation at first
- [ ] ICMP echo request/reply — first true end-to-end milestone once
      Ethernet+ARP+IPv4 exist (answering `ping` from the host)
- [ ] UDP — simpler than TCP, and DHCP needs it; good next milestone
      after ICMP
- [ ] DHCP client — auto-configure IP/gateway/DNS over UDP instead of
      hardcoding an address
- [ ] TCP — connection state machine, sequence numbers, retransmission
      timers, windowing, checksums; by far the biggest single item here
- [ ] Socket syscalls — extend `syscall::OpenFile` (currently
      File/Dir) with a `Socket` variant and wire up
      socket/bind/connect/listen/accept/send/recv, reusing the existing
      fd table
- [ ] DNS stub resolver (A records over UDP) — needs UDP + sockets
- [ ] Loopback interface (127.0.0.1) — doesn't need a NIC driver at
      all, good for testing IPv4/UDP/TCP/sockets in isolation early
- [ ] Gate the whole stack behind a `net` Cargo feature (default-on),
      following the pattern in `Cargo.toml`'s `[features]` section —
      `storage` is the closest existing example of a multi-piece
      feature with its own dependent syscalls

Testing: QEMU's `-netdev user,id=n0 -device rtl8139,netdev=n0` gives host
connectivity (with NAT) without touching real hardware or needing root;
`-netdev tap` for something closer to a real LAN once that's worth it.

## Build System

- [x] Fix linker warning about RWX segment permissions
- [x] Add release build profile with optimizations
- [x] Add GDB support — `make gdb` (named that rather than `make debug`, which was already taken as the debug-ISO build alias) boots QEMU halted with `-s -S`; `debug.gdb` connects, loads the unstripped debug kernel's symbols, and breaks at `kernel_main`. Verified end to end: real argument values, correct source-line stepping through macro-expanded code, and a correct backtrace (`kernel_main` <- `long_mode_start`, the asm entry point)
- [x] `.zed/debug.json` + `.vscode/launch.json`/`tasks.json` — GDB-remote-attach configs mirroring `make gdb`/`debug.gdb`, both with a prelaunch step that backgrounds `make gdb` and polls `:1234` until the gdbstub is listening instead of racing a cold build
- [x] Finish `bad-interrupt-panic` feature — every fatal CPU exception handler (all but the two genuinely continuable ones, `#DB` and `#BP`) now escalates to a real `panic!()` (routed through `panic.rs`'s VGA+serial+stack-trace machinery) instead of a silent `halt_loop()` when the feature is on; also fixed two pre-existing copy-paste bugs in the panic messages (`#TS` said "INVALID RSS", `#AC` said "GENERAL PROTECTION FAULT"). Verified in QEMU: a deliberate `#DE` produces a full panic with file/line and stack trace
- [ ] Consider UEFI boot support alongside legacy BIOS/multiboot2

## Code Quality

- [ ] Add unit tests (where possible in `no_std`)
- [ ] Document public APIs with rustdoc
- [ ] Set up CI (build + boot test)
