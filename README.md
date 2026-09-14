# basicos

A tiny x86_64 kernel written in Rust, built from scratch as a bring-up/learning
project. It boots via Multiboot2 (GRUB), transitions from 32-bit protected
mode to long mode by hand in assembly, then hands off to a `#![no_std]` Rust
kernel that sets up segmentation, interrupts, a syscall path, and a ring-3
jump.

## Features

- **Multiboot2 boot** (`entrypoint.s`) — GRUB-compatible header, CPUID/long-mode
  sanity checks, identity-mapped page tables (1 GiB via 2 MiB huge pages), and
  a manual transition into 64-bit long mode before calling into Rust.
- **GDT + TSS** (`arch/x86_64/gdt.rs`) — kernel/user code & data segments and a
  ring-3 jump (`jump_to_ring3`).
- **IDT + exception handlers** (`arch/x86_64/idt.rs`, `interrupts.rs`) —
  handlers for divide error, breakpoint, double fault, GPF, and page fault,
  plus PIT timer and keyboard IRQs remapped through the legacy 8259 PIC
  (`pic.rs`).
- **SYSCALL/SYSRET** (`arch/x86_64/syscall.rs`, `syscall/mod.rs`) — MSR-based
  fast syscall entry with a small table (`read`, `write`, `exit`, `getpid`,
  `sched_yield`, plus a kernel-log syscall) and `errno`-style return codes.
- **Kernel log** (`log.rs`) — `printk`-style leveled logging (`pr_emerg!` …
  `pr_debug!`) with prefixes, a ring buffer, runtime log-level filtering, and
  independently toggleable VGA/serial sinks.
- **Drivers** (`drivers/`) — VGA text-mode console, 16550 serial port, and a
  PS/2 keyboard driver (scancode set 1 → ASCII).
- **Sync primitives** (`sync/`) — a spinlock and a lock-free ring buffer used
  by the logging subsystem.
- **Usermode stub** (`usermode/`) — a minimal ring-3 entry point used to prove
  the privilege-level jump and syscall path work end to end.

None of this is meant to be production-grade — it's a from-scratch
exploration of what a kernel needs to do to get from firmware to ring 3.

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
| `storage` | ATA driver + FAT32 + initrd, and the syscalls that use them — `open`/`close`/`lseek`/`getdents64`, the file-backed paths of `read`/`write`, and `tree` (implies `heap` + `frame-alloc`) |
| `keyboard` | PS/2 keyboard driver + IRQ1 wiring |
| `serial-input` | Serial (COM1) *input* — output (kernel logging) is always on |
| `pit-100hz` | Reprogram the PIT to 100 Hz; off = its ~18.2 Hz power-on default |
| `apic` | Local APIC driver (`arch::x86_64::apic`) — not wired into boot either way; available infrastructure |
| `usermode-demo` | SYSCALL/SYSRET setup, the syscall dispatch table, and the ring-3 demo program that exercises it |

Build a reduced kernel with `--no-default-features --features <list>`, or
override the Makefile's `FEATURES` variable:

```sh
make FEATURES="--no-default-features --features heap,keyboard"
```

Disabling `usermode-demo` (or `storage` alone, which the demo detects and
skips) is the most visible change — the kernel finishes its boot sequence
and just halts instead of jumping to ring 3. See `[features]` in
`Cargo.toml` for the exact dependency graph.

## Requirements

- Rust **nightly** (pinned via `rust-toolchain.toml`) with `rust-src`
- `grub2-mkrescue` (or `grub-mkrescue`) + `xorriso`, for building the ISO
- `qemu-system-x86_64`, for running it
- Binutils (`as`, `ld`, `strip`) targeting x86_64

The crate builds against a custom target (`x86_64-basicos.json`) using
`-Zbuild-std`, so no installed target triple is required — Cargo builds
`core` and `compiler_builtins` from source (configured in `.cargo/config.toml`).

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

## Project layout

```
entrypoint.s         Multiboot2 header, long-mode bring-up, jump into Rust
linker.ld            Link script (kernel loaded at 1 MiB)
grub.cfg             GRUB menu entry for the ISO
x86_64-basicos.json  Custom bare-metal target spec
src/
  lib.rs             kernel_main: boot sequence, panic handler
  log.rs             printk-style logging + ring buffer
  arch/x86_64/       GDT/TSS, IDT, PIC, SYSCALL/SYSRET, CPUID info, port I/O
  drivers/           VGA, serial, keyboard
  sync/              spinlock, ring buffer
  syscall/           syscall dispatch table
  usermode/          ring-3 entry stub
```

## Status / roadmap

This is a work in progress. See [`todo.md`](todo.md) for the running list —
physical/virtual memory management, a heap allocator, a real scheduler and
process model, storage/filesystem support, and CI are all still open.

## License

No license has been chosen yet.
