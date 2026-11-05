# basicos

A tiny x86_64 kernel written in Rust, built from scratch as a bring-up/learning
project. It boots via Multiboot2 (GRUB), transitions from 32-bit protected
mode to long mode by hand in assembly, then hands off to a `#![no_std]` Rust
kernel that sets up segmentation, interrupts, a syscall path, and a ring-3
jump.

## Features

- **Multiboot2 boot** (`kernel/entrypoint.s`) — GRUB-compatible header, CPUID/long-mode
  sanity checks, identity-mapped page tables (1 GiB via 2 MiB huge pages), and
  a manual transition into 64-bit long mode before calling into Rust.
- **GDT + TSS** (`arch/x86_64/gdt.rs`) — kernel/user code & data segments and a
  ring-3 jump (`jump_to_ring3`).
- **IDT + exception handlers** (`arch/x86_64/idt.rs`, `interrupts.rs`) —
  handlers for divide error, breakpoint, double fault, GPF, and page fault,
  plus PIT timer and keyboard IRQs remapped through the legacy 8259 PIC
  (`pic.rs`).
- **SYSCALL/SYSRET** (`arch/x86_64/syscall.rs`, `syscall/`) — MSR-based
  fast syscall entry and a Linux-numbered, POSIX-flavoured syscall table
  (~90 calls: files, directories, per-process fds, pipes, `poll`, fork/
  exec/wait/kill, clocks, UDP and raw sockets — see "Syscalls" below) with
  `-errno` return codes.
- **Kernel log** (`log.rs`) — `printk`-style leveled logging (`pr_emerg!` …
  `pr_debug!`) with prefixes, a ring buffer, runtime log-level filtering, and
  independently toggleable VGA/serial sinks.
- **Drivers** (`drivers/`) — VGA text-mode console, 16550 serial port, a
  PS/2 keyboard driver (scancode set 1 → ASCII), and an ATA (PIO) disk driver.
- **Storage** — a hand-rolled FAT32 (read/write, no LFN) over any block
  device: the ATA disk, the read-only initrd, or a writable in-memory
  ramdisk formatted from scratch at boot. DOS-style drive letters (`A:` the
  ramdisk, `B:` the initrd, `C:` the disk) pick between them. The format
  itself lives in the `fs/` workspace crate (`basicos-fs`), decoupled from
  the kernel — see "Project layout" below; `kernel/src/fs/` is just the
  hardware glue (the ATA `BlockDevice` impl, mount points, drive letters).
- **Sync primitives** (`sync/`) — a spinlock and a lock-free ring buffer used
  by the logging subsystem.
- **Usermode stub** (`usermode/`) — a minimal ring-3 entry point used to prove
  the privilege-level jump and syscall path work end to end.
- **Scheduler** (`sched/`) — process/task abstraction for ring-0 kernel
  threads, with a real preemptive round-robin scheduler (an xv6-style minimal
  context switch, triggered from the timer IRQ, not just cooperative
  yielding).
- **Init and the shell** — the kernel starts one user process, `C:/INIT.ELF`.
  init starts the shell (`programs/bash`) on the console, restarts it on
  logout, and reaps orphans. Commands (`ls`, `cat`, `ps`, `ping`, ...) are
  ordinary programs in `C:/BIN/`; kernel state reaches them through `/proc`.
  `shell/` is an optional ring-0 debug monitor, off by default.
- **Console** — once booted, the machine is used from the VGA screen and PS/2
  keyboard, with 500 lines of scrollback on Shift+PageUp/PageDown. The serial
  port mirrors it, takes input too (so `-nographic` works), and alone carries
  routine kernel messages after boot.
- **Process isolation** — ring 3 can reach only its own pages, not kernel
  memory or page 0, and a fault in a process kills that process, not the
  kernel. If init itself dies, the kernel panics.
- **ELF loader** (`elf/`) — parses a standalone ET_EXEC/x86_64 binary, loads
  its PT_LOAD segments, and runs it in ring 3. `testprog.s`/`testprog.ld` build a real test binary, entirely
  outside the kernel image, to exercise it against.

- **Networking** (`net/`, `kernel/src/net/`, `drivers/{pci,rtl8139}.rs`) — PCI
  enumeration, a polled RTL8139 NIC driver, and an IPv4 stack (Ethernet,
  ARP, IPv4, ICMP echo, UDP, loopback). The protocol logic is the
  hardware-free `basicos-net` crate, unit-tested on the host; the kernel
  glues it to the NIC. Shell commands `ifconfig`, `arp`, `ping`; a UDP echo
  service on port 7. Static addressing for now (QEMU user-net's
  10.0.2.15/24). No TCP yet.

None of this is meant to be production-grade — it's a from-scratch
exploration of what a kernel needs to do to get from firmware to ring 3.

## Syscalls

Linux x86_64 numbers and struct layouts (`stat`, `dirent64`, `sockaddr_in`,
`pollfd`, `termios`, ...), so C programs written against `<fcntl.h>`/
`<sys/socket.h>` constants work; user pointers are validated (a bad one is
`EFAULT`, not a kernel crash). Each process has its own fd table (64 slots),
inherited across `fork`, with `dup`-shared file descriptions.

| Area | Calls |
|---|---|
| Files | `open`/`openat` (`O_CREAT/EXCL/TRUNC/APPEND/NONBLOCK/CLOEXEC/DIRECTORY`), `close`, `read`/`write`, `readv`/`writev`, `pread64`/`pwrite64`, `lseek`, `stat`/`lstat`/`fstat`/`newfstatat`, `access`, `ftruncate`/`truncate`, `fsync`, `getdents64` |
| Directories | `mkdir`/`mkdirat`, `rmdir`, `unlink`/`unlinkat`, `rename`, `chdir`, `getcwd` (relative paths, `.`/`..`) |
| Descriptors | `dup`/`dup2`/`dup3`, `fcntl` (`F_DUPFD`, `F_GETFD/SETFD`, `F_GETFL/SETFL`), `ioctl` (tty queries, `FIONREAD`), `pipe`/`pipe2`, `poll`, `umask` |
| Processes | `fork`, `execve`, `wait4`, `exit`/`exit_group`, `getpid`/`getppid`, `kill` (fatal signals terminate; no handlers), `sched_yield`, `brk`, anonymous `mmap`/`munmap`, `getuid` family (always root) |
| Time / system | `clock_gettime`/`clock_getres`, `gettimeofday`, `time`, `nanosleep`/`clock_nanosleep`, `uname`, `sysinfo`, `getrandom` |
| Sockets (UDP, raw IP) | `socket`, `bind`, `connect`, `sendto`, `recvfrom`, `getsockname`/`getpeername`, `setsockopt` (`SO_RCVTIMEO`)/`getsockopt`, `shutdown`; `read`/`write`/`poll` work on them too |
| basicos-specific | `klog` (0x400), `tree` (0x401), `kpanic` (0x402) |

Devices: `/dev/null`, `/dev/zero`, `/dev/random`/`urandom` (xorshift — not
cryptographic), `/dev/tty`/`/dev/console`; fds 0/1/2 start on the console,
with a cooked-mode line discipline for stdin (echo, backspace, Ctrl-D EOF).

What isn't there (yet): signal *handlers* (`rt_sigaction`), threads/`futex`,
TCP (`SOCK_STREAM` is `ESOCKTNOSUPPORT`), file-backed `mmap`, `select`/
`epoll`, long file names, real timestamps in `stat`.

`make posix-test` runs `programs/posix` — a self-checking program with ~100
checks over all of the above — in headless QEMU; `make net-test` covers the
NIC and sockets end to end.

## Cargo features

Every optional subsystem is gated behind a Cargo feature, **all on by
default** — plain `cargo build` / `make` gets the full kernel. Disabling a
feature actually removes that code from the build (not just from the boot
sequence): unmask the matching PIC line and nothing fires it, remove the
module and nothing references it.

| Feature | Gates |
|---|---|
| `heap` | `mem::heap`, the `#[global_allocator]` — pulls in `alloc` |
| `frame-alloc` | `mem::frame`, the physical frame allocator |
| `paging` | `mem::paging`, the virtual memory manager (implies `frame-alloc`) |
| `storage` | ATA driver + FAT32 + initrd + a writable in-memory ramdisk (`A:`/`B:`/`C:` — see below), and the syscalls that use them — `open`/`close`/`lseek`/`getdents64`, the file-backed paths of `read`/`write`, and `tree` (implies `heap` + `frame-alloc`) |
| `keyboard` | PS/2 keyboard driver + IRQ1 wiring |
| `serial-input` | Serial (COM1) *input* — output (kernel logging) is always on |
| `pit-100hz` | Reprogram the PIT to 100 Hz; off = its ~18.2 Hz power-on default |
| `apic` | Local APIC driver (`arch::x86_64::apic`) — not wired into boot either way; available infrastructure |
| `usermode-demo` | SYSCALL/SYSRET setup, the syscall dispatch table (per-process fd tables, pipes, ...), the RTC, and the ring-3 demo program that exercises it (implies `heap`) |
| `scheduler` | Process/task abstraction + preemptive round-robin scheduler for ring-0 kernel threads (`sched/`) — takes over kernel_main's boot tail from `usermode-demo` when both are on |
| `shell` | Ring-0 debug monitor (`shell/`), **off by default**: it competes with the userspace shell for console input. Implies `scheduler`, since it runs as a task |
| `net` | PCI + RTL8139 driver + the `basicos-net` IPv4 stack, polled by a `net` scheduler task (implies `heap` + `frame-alloc` + `scheduler`) |
| `elf-loader` | ELF64 loader + one-shot ring-3 exec (`elf/`), used to start init and by `execve` (implies `usermode-demo` + `storage`) |

Every combination is meant to build — `make feature-check` compiles a
matrix of them (none, each alone, all-but-one) so a broken gate is caught.
Build a reduced kernel with `--no-default-features --features <list>`, or
override the Makefile's `FEATURES` variable:

```sh
make FEATURES="--no-default-features --features heap,keyboard"
```

`make LOG_LEVEL=warn` compiles out kernel messages less serious than that
(`emerg`, `alert`, `crit`, `err`, `warn`, `notice`, `info`, `debug`). They
leave no code or strings behind, and so don't appear in `dmesg` either. By
default debug builds keep everything and release builds drop `pr_debug!`.

With `scheduler` on (the default), the kernel starts `C:/INIT.ELF` (with
`elf-loader`), and everything else in userspace comes from init, instead of
running the fixed `usermode-demo` program — the two don't compose (see
`sched`'s module docs for why). Programs are real, preemptible scheduler
tasks with their own address spaces. With neither `elf-loader` nor `shell`, two demo
tasks show preemption. Disabling both `scheduler`
and `usermode-demo` is the most visible change from there — the kernel
finishes its boot sequence and just halts. See `[features]` in
`kernel/Cargo.toml` for the exact dependency graph.

## Requirements

- Rust **nightly** (pinned via `rust-toolchain.toml`) with `rust-src`
- `grub2-mkrescue` (or `grub-mkrescue`) + `xorriso`, for building the ISO
- `qemu-system-x86_64`, for running it
- Binutils (`as`, `ld`, `strip`) targeting x86_64
- A host C compiler (`gcc` or `clang`), only if you're building `userland/`

### Writing a Rust program for basicos

```sh
make new-prog NAME=foo      # scaffolds programs/foo (8.3-safe names only)
$EDITOR programs/foo/src/main.rs
make disk && make run       # then in the shell: foo
```

A program is `#![no_std]`/`#![no_main]`, calls `basicos_sdk::entry!(main)`,
and `use basicos_sdk::prelude::*;` — see `programs/` for examples, and
`sdk/src/lib.rs` for what's available.

The `kernel` crate builds against a custom target
(`kernel/x86_64-basicos.json`) using `-Zbuild-std`, so no installed target
triple is required — Cargo builds `core` and `compiler_builtins` from source
(configured in `kernel/.cargo/config.toml`). The sibling `fs` crate is an
ordinary host-target crate — see "Project layout" below.

## Building & running

```sh
make            # build a debug ISO (build/basicos.iso)
make run        # build + boot the debug ISO in QEMU, serial → stdio

make release    # build an optimized, stripped ISO
make run-release

make size       # print kernel/library binary sizes
make clean      # remove build/ and target/
```

Kernel log output goes to both the VGA console and the serial port; `make
run` wires QEMU's serial to your terminal (`-serial stdio`) so `pr_info!` etc.
show up there.

### Reproducible build in a container

Don't want to install the toolchain (nightly Rust, GRUB, mtools, ...)? One
script builds everything inside a container and drops a self-contained
release folder on the host:

```sh
./docker-build.sh                        # -> ./release/
./docker-build.sh -o /some/dir           # somewhere else
./docker-build.sh --features heap,keyboard   # reduced kernel (see Cargo features)
./docker-build.sh --no-cache             # force a full rebuild
./docker-build.sh --no-sign              # skip the GPG signature
```

It uses Docker if the daemon is reachable, otherwise rootless Podman
(`ENGINE=podman|docker` to force one). The release folder contains
`basicos.iso` (bootable), `kernel.bin` (stripped), `initrd.img`,
`testprog.elf`, `disk.img.gz` (the 2 GiB `C:` test disk, gzipped — it's
almost all zeros), `run.sh`, `BUILD-INFO.txt`, `SHA256SUMS`, and, when
signed, `SHA256SUMS.asc` and `basicos-signing-key.asc`. To try it:

```sh
cd release && ./run.sh        # needs qemu-system-x86_64
```

Releases are signed on the host after the build, so the private key never
enters the container; the script stops before building if the key isn't
available. `SIGNING_KEY=<id>` picks a different key, and
`tools/sign-release.sh <dir>` signs a folder again. To verify one:

```sh
gpg --import basicos-signing-key.asc
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum --check SHA256SUMS
```

The signing key's fingerprint is
`E54C 1041 23A2 2A44 D269  56F7 4B86 F8E2 8888 92DC`. Compare it with what
`gpg --verify` reports: the key in the folder only proves a match with
itself, so anyone who altered a release could swap that too.

The container pins a specific dated nightly (this kernel uses unstable
features, so a floating `nightly` can break it); bump it with
`RUST_TOOLCHAIN=nightly-YYYY-MM-DD ./docker-build.sh` when you're ready to.

## Project layout

A Cargo workspace (root [`Cargo.toml`](Cargo.toml) is a virtual manifest —
`[workspace] members = ["kernel", "fs", "net", "sdk", "init", "programs/*"]`), plus non-Cargo pieces that build
through the top-level `Makefile`:

```
kernel/                 The OS itself — a #![no_std] crate, custom target
  entrypoint.s          Multiboot2 header, long-mode bring-up, jump into Rust
  linker.ld             Link script (kernel loaded at 1 MiB)
  grub.cfg              GRUB menu entry for the ISO
  x86_64-basicos.json   Custom bare-metal target spec
  .cargo/config.toml    build-std, the target above, force-frame-pointers
  src/
    lib.rs              kernel_main: boot sequence, panic handler
    log.rs               printk-style logging + ring buffer
    arch/x86_64/         GDT/TSS, IDT, PIC, SYSCALL/SYSRET, CPUID info, port I/O
    drivers/             VGA, serial, keyboard, ATA
    fs/                  Kernel-side glue only (see fs/ below): the ATA
                         BlockDevice impl, mount points, A:/B:/C: drive
                         letters (fs::resolve_path)
    mem/                 frame allocator, paging (+ per-process address
                         spaces), heap allocator, multiboot2
    sync/                spinlock, ring buffer
    syscall/             syscall dispatch table (read/write/fork/execve/
                         wait4/mmap/munmap/brk/...)
    usermode/            ring-3 entry stub (usermode-demo)
    sched/               process/task abstraction, round-robin scheduler,
                         fork/exec/wait support
    shell/               ring-0 debug monitor (off by default)
    elf/                 ELF64 loader

fs/                     `basicos-fs` — the FAT32 format, decoupled from the
                        kernel (see fs/src/lib.rs) so it's usable both in
                        the kernel and, eventually, from a host-side
                        mkfs/fsck-equivalent tool outside it. An ordinary
                        host-target crate — no custom target, no build-std.
  src/
    fat32.rs             Parsing, directory walks, read/write
    ramdisk.rs           From-scratch FAT32 "mkfs" over a BlockDevice
    block.rs             The BlockDevice trait + MemDisk

net/                    `basicos-net` — the IPv4 stack as a sans-IO state
                        machine (Ethernet, ARP, IPv4, ICMP, UDP, loopback;
                        `Stack` + the `NetDevice` trait the NIC driver
                        implements). Host-target, no_std, unit-tested with
                        a bare `cargo test`; `make net-test` boots the
                        kernel and checks it end to end against QEMU.

sdk/                    `basicos-sdk` — the Rust userspace SDK (there is no
                        `std` for basicos; this is core + alloc + the
                        kernel's syscalls): typed syscall wrappers (fs,
                        process, mem, io), a global allocator so Vec/
                        String/Box work, the entry! macro (emits the
                        _start shim), a panic handler, println!. Also the
                        userspace toolchain itself: config.toml (target,
                        build-std, static relocation) and link.ld (load
                        address), passed to cargo with --config.
init/                   The first userspace program, built on the SDK.
                        `make init-elf`; on the disk as C:\INIT.ELF.
programs/               Every other Rust userspace program, one crate each
                        (hello, ls, heap, forkwait, posix — the syscall
                        self-test, udpecho — a UDP echo server). `make new-prog
                        NAME=foo` scaffolds one; everything here is built
                        and copied onto the disk as C:\BIN\<NAME>.ELF by
                        `make disk`, no Makefile edit needed.

tools/                  Test drivers: `feature-check.sh` (feature matrix),
                        `shell-test.py`, `vga-test.py`, `posix-test.py`, `ext4-test.py`, `net-test.py` (headless QEMU runs;
                        `qemu-shell.py` is their shared helper).

userland/               Freestanding C programs against the kernel's own
                        syscall ABI (syscalls.h) — not a Cargo crate; see
                        userland/Makefile. Copied onto the test disk's
                        C:\TESTPROG\ by `make disk`.

testprog.s/.ld,
forktest.s/.ld          Standalone test ELF binaries, hand-written in
                        assembly, for exercising elf::load without a C
                        toolchain — copied onto the test disk as
                        /hello.elf and /forktest.elf.
```

## Status / roadmap

This is a work in progress. See [`todo.md`](todo.md) for the running list —
a real filesystem-backed shell, per-process address spaces, and fork/exec/
wait/mmap/brk all exist now; CI and networking are still open.

## License

No license has been chosen yet.
