# basicos TODO

## Logging System Enhancements

- [x] Reprogram PIT to 100Hz for accurate timestamps (currently using default ~18.2Hz)
- [x] `pr_cont!` for continuation lines, opened by `pr_start!` since `pr_info!` and friends end their own line. A message from another task ends the open line rather than joining it, and a continuation that lost its line starts a new one
- [x] `dmesg` command, reading the ring buffer through `/proc/dmesg`
- [x] Compile-time log level filtering: `make LOG_LEVEL=warn` strips anything less serious, code and strings included. Default is `debug` in debug builds, `info` in release
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
- [x] Implement scheduler (round-robin to start) — real preemption: an xv6-style minimal context switch (callee-saved regs + RSP only), triggered every PIT tick from the timer IRQ, not just cooperative yielding. Verified by two tasks that never call `yield_now()` themselves, whose output still interleaves. Found and fixed two real bugs along the way: RFLAGS.IF isn't part of the saved context (it's a single CPU-wide flag, not per-task), so a task preempted mid-interrupt-handler was leaving IF stuck at 0 for whatever ran next — fixed by an unconditional `sti` inside `switch()` (**later replaced** — see the next item); and `log.rs` wasn't holding a lock across a whole log line (just the ring-buffer write), so two tasks' output could interleave mid-word once real preemption existed to expose it — fixed by disabling interrupts for each `pr_*!`/`kprint!` call's full duration
- [x] Scheduler race: `switch()`'s blanket `sti` let a timer tick land while the CPU was in scheduler context (or mid-switch into it), where `CURRENT` says a task is running but none is. `yield_now` then saved the *scheduler's* RSP into that task's slot, and the next switch into it popped garbage off the boot stack and `ret`ed into `.bss` (`INVALID OPCODE` right after `SCHED_RSP`). Latent since the `sti` was added; exposed once init was the *only* task, sitting in a tight `yield` loop so the scheduler switched constantly (the shell and demo tasks had been diluting it). Found by tracing QEMU's exceptions (`-d int`): the tick was delivered on the instruction right after `switch`'s `sti`, with SP on the boot stack. Fixed properly rather than patched: `switch()` now saves/restores RFLAGS (`pushfq`/`popfq`) so each context resumes with its own interrupt state, new tasks start with an `INITIAL_RFLAGS` slot, and `run()` executes with interrupts off, using an atomic `sti; hlt` only to idle. Verified: six ~700-tick boots of init alone (previously dead in 20 ms) plus the full fork/exec/mmap/file-I/O regression with the shell and demo tasks running alongside
- [x] Scheduler race #2: `yield_now` marked the task `Ready` and *then* switched, with interrupts still on. A timer tick in that window ran a nested `yield_now`, which switched away, got the task rescheduled (`Running` again), and resumed the outer call — which switched away a second time still marked `Running`, so `pick_next` (Ready-only) never ran the task again and the scheduler idled forever. Needs a task that yields constantly to hit reliably; found when the `net` polling task went silent a few seconds after boot (RIP sampled through the QEMU monitor sat in `sched::run`'s `hlt`: no Ready tasks). Fixed by disabling interrupts from before the state change until after the switch returns (restoring the caller's IF afterwards). Verified: 3 idle 10 s boots, plus ~22k UDP round-trips through the net task. (This fix had gone missing from `yield_now` and has been put back)
- [x] Intermittent crash in `forkwait` (fork + exec), about one boot in two: ring-3 code jumping to `0x3` or faulting in `hello`. `execve` found its syscall frame through a global that every syscall overwrote, and it read that global only after copying argv in, long enough for a tick to let another process (init, spinning on `wait`) overwrite it. `execve` then redirected the wrong process and returned into its own unmapped image. The IVT pattern in the old notes was a clue to a second problem: page 0 was mapped and user-readable. Fixed by deriving the frame from the running task's own kernel stack (`arch::x86_64::syscall::current_frame`), so there's no global to overwrite. Measured with 3 runs per boot: 6/12 boots failed before, 0/12 after
- [x] Per-process address spaces — `mem::paging`'s process zone: `entrypoint.s`'s boot-time 2 MiB huge pages covering a fixed 16 MiB virtual slice (`PROC_ZONE_START`) get demoted to real 4 KiB page tables once, at boot (`demote_process_zone`), so each process (`PageMapper::new_process_space`/`fork_from`) can get its own private physical backing there — code, heap, mmap arena, and stack all live inside it — while everything else (kernel code/data/heap, page tables themselves) stays the single shared mapping every address space inherits by construction. Physical range reserved from `frame::allocate()` up front, which is what makes it sound for kernel code to `memcpy` into a freshly allocated frame via a raw physical pointer regardless of whose CR3 is currently loaded
- [x] Per-task kernel stack + `TSS.RSP0`/syscall-stack switching — closes the exact gap the scheduler used to have: `run()` now points `TSS.RSP0` (`gdt::set_kernel_stack`) and `syscall_entry`'s own stack target (`arch::x86_64::syscall::set_kernel_stack_top`) at whichever task is about to run, and reloads CR3 too, on *every* switch — a ring-3 process's own syscalls and any interrupt that catches it in ring 3 land on its own stack, not a shared one
- [x] ELF loader for userspace binaries — `elf::load_into`: parses ET_EXEC/EM_X86_64, copies PT_LOAD segments into a process's own address space (via its page-table walk, not a raw physical pointer — correct regardless of which CR3 is loaded). `elf::spawn`/`sched::spawn_process` load a binary and queue it as a real scheduler task (not a one-way trip anymore). Verified against two standalone binaries built completely outside the kernel: `testprog.s` (`make testprog` → `/hello.elf`) and `forktest.s` (`make forktest` → `/forktest.elf`, see below)
- [x] `fork()`/`execve()`/`wait4()` — real process duplication and image replacement, not stubs. `fork()` deep-copies the calling process's address space (`PageMapper::fork_from`, full copy, not copy-on-write) and splices a copy of its in-flight syscall register frame onto the child's own stack (`arch::x86_64::syscall::fork_child_entry`/`SyscallFrame`) so it resumes exactly where the parent called `fork()`, with RAX forced to 0. `execve()` tears down the caller's own memory (`PageMapper::unmap_zone`) and loads a fresh ELF into the same address space, redirecting the in-flight syscall's saved RIP/RSP instead of returning to the old program. `wait4()`/`sched::wait_for_child` blocks (a plain yield loop) until a zombie child shows up, reaps it, and returns its exit status. All four verified together end to end in QEMU via `forktest.elf`: fork → child execs into `hello.elf` (proving the teardown/reload is real, not a no-op) → child exits → parent reaps it via `wait4` — and `meminfo` shows byte-identical free frame counts before and after, confirming the address-space cleanup on `exit()`/`execve()` doesn't leak
- [x] `mmap()`/`munmap()`/`brk()` — anonymous-only (no page cache, so no file-backed mappings — `MAP_ANONYMOUS` or `EINVAL`); `mmap` bump-allocates from a per-process mmap arena, `brk` grows/shrinks a per-process heap region, both backed by real `alloc_and_map`/`unmap_and_free` calls into the calling process's own address space. Verified in `forktest.elf`: grows the heap, writes/reads a distinct value through the new page, maps an anonymous page, writes/reads a different value through *that*, unmaps it
- [x] Basic shell — `shell::shell_task`, runs as a scheduler task; `help`/`echo`/`clear`/`uptime`/`ps`/`meminfo`/`ls`/`cat`/`run`. Reads from serial if available, else keyboard. Enabling `scheduler` supersedes `usermode-demo`'s boot-time ring-3 jump (see `Cargo.toml`) — the two don't compose, and a shell is more useful as the default than a fixed demo. `run` now spawns a real background process (`elf::spawn`) instead of a one-way jump — the shell keeps running concurrently
- [x] Zombies from kernel-started processes — anything the kernel starts is now init's child (`spawn_process`), so init reaps it. `MAX_TASKS` is 32
- [ ] `fork()` is a full deep copy of the process's address space, not copy-on-write — correct, but means a large process's `fork()` is O(its memory), same as the mmap arena's "no reuse on `munmap`" simplification elsewhere. COW would need write-protecting the shared pages and a real page-fault handler integration; not done here
- [x] Per-process file descriptor tables — see Syscalls
- [x] A fault in ring 3 kills only that process (`interrupts::kill_if_user`), with status 128 + the signal the shell reports. Faults in ring 0 still halt. Verified by `userland/segv.c` and `programs/isotest` via `make shell-test`
- [x] User/kernel memory isolation — `paging::protect_kernel_mappings` clears the user bit on everything outside the process zone, so ring 3 can only reach its own pages; page 0 is unmapped entirely. Mutation-checked with `make shell-test`
- [x] Kernel heap: it never merged freed blocks, so traffic shredded it until a small allocation failed (a release-build panic after ~65k UDP datagrams), and it took nearly all of RAM on some builds, leaving `exec` short of frames. Now an address-ordered list with coalescing, capped at half of free memory. Verified by 154k datagrams through `make net-test` on the release kernel
- [ ] A process that hits the frame limit can panic the kernel: `fork_from`, `new_process_space` and `setup_stack` `.expect()` their allocations instead of returning `ENOMEM`. Reachable with a fork bomb of large processes
- [ ] User pages are all RWX: no NXE, and ELF segments share pages (`sdk/link.ld` doesn't page-align them), so text isn't read-only and stacks are executable. SMEP/SMAP aren't enabled either
- [ ] No guard pages on the 16 KiB kernel stacks: an overflow silently corrupts the neighbouring task's stack

### Two real bugs found building `userland/` (a C toolchain against these syscalls)

Both were silent, not crashes — the kind that only show up once something exercises the exact pattern, which is exactly what happened building real (not hand-assembled) test programs against this syscall ABI for the first time:

- **`current_frame()` read too late.** `arch::x86_64::syscall::current_frame()` reads a single global `syscall_entry` overwrites on *every* syscall, from *any* task. `sched::fork_current`/`exec_current` both used to read it only after their own lengthy, fully-preemptible work (`fork_from`'s deep copy, `load_into`/`setup_stack`'s many `frame::allocate()` calls) — if a timer tick preempted mid-way and some *other* task made a syscall of its own before this one resumed, the late read silently picked up that other task's frame pointer, and `exec_current` would overwrite an unrelated process's saved RIP/RSP. Manifested as an intermittent page fault at a process-zone address, only when enough concurrent processes existed to make the race likely. Fixed by capturing the pointer at the very top of both functions, before any preemptible work — see both functions' doc comments and `current_frame()`'s own (now much longer) one.
- **GCC silently dropped a syscall argument.** `userland/syscalls.h`'s raw `__syscallN` wrappers are `static`, `noinline`, fully visible in their translation unit — and GCC's interprocedural constant propagation (`-O2`) clones a specialized copy per distinct constant syscall number (e.g. `__syscall1.constprop.0` for `close`'s always-3 `SYS_CLOSE`), and in the clone, "proved" the discarded read-write argument operand was dead and deleted the instruction that loaded it into its register entirely — so `sys_close(fd)` silently executed with whatever register value was already there from the *previous* syscall (observed: `close`'s fd argument arriving as the *previous* `write` call's byte count). `noinline` alone doesn't stop this — it's IPA cloning, a different optimization. Fixed with `__attribute__((noipa))`, which disables that cloning outright. See `syscalls.h`'s header comment and the comment above the `__syscallN` declarations for the full story.

## Init (`init/`)

- [x] `init/` compiles to a correct ELF for this kernel's loader — a `#![no_std]`/`#![no_main]` Rust crate, third workspace member. What "correct" took (now factored into the SDK, see the next item): reusing `kernel/x86_64-basicos.json` (userspace has the same no-SSE/no-red-zone/abort constraints) plus `relocation-model=static` (the loader rejects `ET_DYN` and does no relocation; the target's default model is PIC), a linker script pinning the load address to `PROC_CODE_START` (0x600000), a `_start` shim (process entry isn't call-shaped — same job as `userland/crt0.s`), and raw syscall wrappers that declare every argument register `inout(..) => _` because this kernel's syscall return path doesn't restore them. Verified with `readelf`/`objdump`: `ET_EXEC`/`EM_X86_64`, both `PT_LOAD`s inside the loader's window and nothing below 0x600000, zero relocations, no GOT, zero SSE/x87 instructions. Verified at runtime: `run C:/init.elf` — correct pid, `argc`/`argv[0]` through the shim, `getpid`/`write`/`wait4`/`yield` all working; and the panic path (injected a panic: message + location on the console, kernel reports exit status 101). `make init-elf` builds it, `make disk` puts a debug-stripped copy (134 KB vs 2.8 MB — the kernel reads the whole file into a heap buffer per exec) on the disk as `C:/INIT.ELF`. Root `Cargo.toml` gained `default-members = ["fs"]`: a bare `cargo build` there would otherwise try to build `kernel`/`init` for the host, and `init` fails to link (it supplies its own `_start`)
- [x] Rust userspace SDK + toolchain (`sdk/`, `programs/`) — init's inlined runtime generalized so writing a Rust program for basicos is `make new-prog NAME=foo`. `basicos-sdk`: typed wrappers over every syscall (`fs::File`/`read_dir`, `process::{fork,exec,wait,exit}`, `mem::{brk,mmap}`, `io`/`print!`), an `entry!` macro (emits `_start` into the *program's* object — defining it in the SDK relies on the linker happening to pull in that archive member), and a `#[global_allocator]` (address-ordered first-fit free list with coalescing, grows via `brk` then falls back to `mmap`) so `Vec`/`String`/`Box`/`BTreeMap` work. The toolchain proper is `sdk/config.toml` + `sdk/link.ld`, passed to cargo with `--config` from the repo root (one definition, no `.cargo/` per crate). `make disk` discovers `programs/*` and puts each on the disk as `C:/BIN/<NAME>.ELF`. Verified under the kernel: `hello`, `ls` (`read_dir` via `getdents64`), `forkwait` (fork → exec with argv → wait → `ECHILD`), and `heap`, an allocator stress test (100k-element Vec growth, over-aligned boxes, 128 MiB of churn through ~11 MiB of address space, coalescing 4 MiB of small blocks back into a 3 MiB one)
- [ ] There is no `std` for basicos, and this SDK is deliberately not a stand-in for one. A real `x86_64-unknown-basicos` target with `std` would mean a `library/std/src/sys/basicos` backend (fs, process, time, env, stdio — most of which map onto syscalls the SDK already wraps), a target upstreamed or carried as a rustc fork, and a kernel with what std assumes: threads, a clock (`Instant`/`SystemTime`), `getrandom`, environment variables, and real file metadata. Today's kernel has none of the first four
- [ ] SDK gaps: single-threaded allocator (no lock — fine while a process has one thread), no environment variables (the kernel ignores `envp`), no time, and `File` buffers whole files in the kernel until `close` (a kernel limitation, documented on the type)
- [x] The kernel boots init as pid 1, and init starts the shell (`programs/bash`) on the console and restarts it on logout. Every kernel-shell command is now a program in `C:/BIN/`; `/proc` (`syscall/procfs.rs`) gives them the state only the kernel has. The ring-0 shell stays as an off-by-default debug monitor (`shell` feature). If init exits, the kernel panics; it can't be killed
- [x] Orphans are reparented to init on exit and kill, and init reaps them (`programs/isotest`)

## Userland C toolchain (`userland/`)

- [x] A real C compiler pointed at basicos's syscall ABI, not just hand-written assembly test programs — `userland/syscalls.h` (typed wrappers + raw `__syscallN` primitives, matching `syscall::nr`/`open_flags`/`whence`/`mmap_flags`/`tree_target`/`errno`), `userland/crt0.s` (the one piece that can't be C — a process's first instruction isn't call-shaped, see its header comment), `userland/user.ld` (shared linker script, same fixed `0x600000` load address as `testprog.ld`/`forktest.ld`), and `userland/Makefile` (`-ffreestanding -nostdlib -nostdinc -static -fno-pic -fno-pie -fno-stack-protector -fcf-protection=none -mno-red-zone -mgeneral-regs-only` — the last one is load-bearing, not style: the scheduler's context switch never saves/restores XMM/x87/MMX state, so ordinary C code reaching for SSE by default, as gcc does on x86_64, would silently corrupt it across a preemption). Wired into the top-level `Makefile` (`make userland`, and `make disk`/`make disk-reset` build + copy it automatically) and the test disk as `/chello.elf`, `/fileio.elf`, `/forkexec.elf`
- [x] Three example programs, each exercising syscalls the assembly test programs didn't: `hello.c` (write/exit, the C equivalent of `testprog.s`), `fileio.c` (open/write/read/close — a real round trip through the ramdisk), `forkexec.c` (brk/mmap/munmap, then fork/execve/wait4 — execve's into the *assembly*-built `/hello.elf` on purpose, to prove a C-compiled and a hand-assembled process interoperate through the same ABI). All three boot-verified in QEMU; building and running them is what surfaced the two bugs above

## VGA / Display

- [x] The VGA screen and PS/2 keyboard are the main console: the line discipline takes input from the keyboard as well as serial and echoes to both, control characters and UTF-8 are handled, lines wrap correctly, and after boot only warnings and worse reach the screen. Shift+PageUp/PageDown scroll 500 lines of history. Tested by `make vga-test`, which presses real keys through QEMU's monitor and reads VGA memory
- [ ] Add color support to VGA driver (currently hardcoded light grey on black)
- [ ] Line editing and command history: arrow keys are discarded for now
- [ ] ANSI escape sequences on output: programs can't position the cursor or set colors on the screen
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

## Syscalls

- [x] Linux-numbered POSIX-ish syscall layer — per-process fd tables (inherited across `fork`, `dup`-shared descriptions, close-on-exec), pipes, `poll`, devices (`/dev/null` etc.), the console as fd 0/1/2 with a cooked-mode stdin, files and directories (`stat`, `mkdir`, `rmdir`, `unlink`, `rename`, `chdir`/`getcwd`, `pread`/`pwrite`, `truncate`, `fsync`, `openat` family), clocks and `nanosleep` (RTC-seeded wall clock), `kill` (fatal signals only), `uname`/`sysinfo`/`getrandom`, and UDP sockets. User pointers are validated (`EFAULT`). The FAT32 crate gained `create_dir`/`remove_dir`/`rename` (host-tested). Verified by `programs/posix` (~100 self-checks, mutation-checked) via `make posix-test`, and by `make net-test` for sockets over the real NIC. Kernel: `kernel/src/syscall/{mod,fd,file,io,sock,misc,console,uaccess}.rs`
- [x] Every Cargo feature combination builds (`make feature-check`: none, each alone, all-but-one). Fixed: `kernel_main` had no diverging tail without `elf-loader`; the page-fault handler used `format!` (needs the heap); `usermode-demo` now implies `heap`
- [ ] Signal handlers (`rt_sigaction`/`rt_sigprocmask`/`sigreturn`) — needs a per-process pending mask and a trampoline that runs on the way back to user mode; today `kill` can only terminate
- [ ] Threads (`clone` with `CLONE_VM`, `futex`, `arch_prctl` for TLS/`%fs`) — a libc port needs these; tasks currently can't share an address space
- [ ] `select`/`epoll`, and blocking waits that sleep instead of busy-yielding (needs a blocked task state — the same gap as interrupt-driven NIC RX)
- [ ] File-backed `mmap`; `MAP_FIXED`/address hints for anonymous `mmap`
- [ ] Open files are buffered whole in memory per *open*: two independent opens of one file don't see each other's writes until close (last close wins), and unlinking a file that's open with unflushed writes recreates it on close. A shared per-file cache keyed by (drive, path) would fix both
- [ ] Long file names (FAT32 LFN), real timestamps for `stat` (FAT has them; the parser ignores them), permissions/uids
- [x] Raw sockets — `AF_INET`/`SOCK_RAW` for any IP protocol from 1 to 254, Linux semantics: packets arrive whole (IP header first) as well as being handled by the stack, and you send the payload. No `IP_HDRINCL`, so the source address can't be forged. `ping` is built on them. Tested in `net/src/stack.rs`, `programs/posix` and `make shell-test`
- [ ] `sendmsg`/`recvmsg`, `socketpair`, `AF_UNIX`

## Networking

Roughly bottom-up order — each step needs the one before it to test
against. `fs::BlockDevice` (device-agnostic driver behind a small trait)
is the template to follow for the NIC layer too.

**Done so far** (branch `ip-stack`): everything up to and including UDP
and loopback. The protocol logic is a sans-IO state machine in the `net/`
crate (`basicos-net`: `Stack` enqueues outgoing frames, callers feed it
received ones and a clock; 34 host unit tests, run with a bare `cargo
test`), and the kernel supplies PCI enumeration (`drivers/pci.rs`), a
polled RTL8139 driver (`drivers/rtl8139.rs`) implementing the `NetDevice`
trait, and a `net` scheduler task (`net/mod.rs`) that pumps the two
together, self-tests against the gateway at boot, and runs a UDP echo
service on port 7. Shell: `ifconfig`, `arp`, `ping <ip> [n]`. Static
addressing (10.0.2.15/24 via 10.0.2.2, QEMU's user-net defaults) for now.
Verified against the real driver in QEMU (`make net-test`): gateway ARP +
ping, and thousands of UDP datagrams (0–1472 bytes) forwarded in from the
host and echoed back byte-for-byte, with tcpdump finding no bad checksums.

Follow-ups on what's built:
- [ ] Interrupt-driven RX — the driver runs with IMR=0 and the `net` task
      spins on `yield_now`, burning a scheduler slot forever; wire the
      NIC's IRQ line (PCI interrupt line -> PIC) and have the task sleep
      until it fires (needs a blocked task state, which `sched` lacks)
- [ ] IP fragmentation/reassembly (fragments are currently dropped) and
      a real routing table (currently: on-subnet, else the gateway)
- [ ] ICMP errors beyond port-unreachable (TTL exceeded needs forwarding)

- [x] PCI bus enumeration (config space via ports 0xCF8/0xCFC, or MMIO
      ECAM) — needed just to find a NIC's BARs/IRQ line in the first place
- [x] NIC driver — RTL8139 is the classic simple one to start with
      (QEMU's `-device rtl8139`); e1000 or virtio-net are the more
      "real" alternatives once the stack above it works. RX/TX
      descriptor rings need physically-contiguous DMA buffers —
      `mem::frame::allocate_contiguous` already exists for this
- [x] `NetDevice` trait (mirrors `fs::BlockDevice`) so the protocol
      stack doesn't care which NIC driver is underneath — send/recv a
      raw Ethernet frame, report a MAC address and MTU
- [x] Packet buffer management — a pool of fixed-size buffers (or just
      `alloc::vec::Vec<u8>` for a first pass) with headroom to prepend
      headers as a packet moves down the stack
- [x] Ethernet layer — frame parsing/construction (dst/src MAC,
      EtherType), dispatch to ARP vs IPv4 by EtherType
- [x] ARP — request/reply + a small IP-to-MAC cache; every IPv4 send
      needs this before it can address a frame
- [x] IPv4 — header parse/build, checksum, a routing table (at minimum
      "local subnet vs. default gateway"); fine to just drop anything
      that would need fragmentation at first
- [x] ICMP echo request/reply — first true end-to-end milestone once
      Ethernet+ARP+IPv4 exist (answering `ping` from the host)
- [x] UDP — simpler than TCP, and DHCP needs it; good next milestone
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
- [x] Loopback interface (127.0.0.1) — doesn't need a NIC driver at
      all, good for testing IPv4/UDP/TCP/sockets in isolation early
- [x] Gate the whole stack behind a `net` Cargo feature (default-on),
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
