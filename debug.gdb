# GDB helper for `make gdb`. Connects to QEMU's gdbstub, loads symbols
# from the debug kernel, and stops at kernel_main — QEMU is halted at
# the literal reset vector when this connects (16-bit real mode, before
# even the BIOS runs), so without a breakpoint `continue` would run
# straight through BIOS/GRUB boot with nothing to see.
#
# Usage:
#   1. make gdb          (in one terminal — QEMU waits, halted)
#   2. gdb -x debug.gdb  (in another)
#   3. continue           (runs BIOS -> GRUB -> our own boot, stopping
#                          right as kernel_main starts)
#
# From there, normal GDB: `next`/`step`/`continue`, `break <function>`,
# `print`, `bt` for a backtrace (works because -C force-frame-pointers=yes
# is set kernel-wide — same thing that makes panic.rs's own stack traces
# possible). `p/x $rsp`, `x/10gx <addr>` etc. for raw memory, same as any
# other GDB session.
#
# If a breakpoint or `bt` looks wrong right after a privilege-level
# change (entering/leaving ring 3, e.g. around gdt::jump_to_ring3 or a
# syscall), that's QEMU's gdbstub catching up to the CPU mode switch,
# not a real problem — `stepi` through it once and things resync.

file build/kernel.bin
set architecture i386:x86-64
set disassembly-flavor intel
target remote localhost:1234
break kernel_main
echo \n---\nConnected, halted before BIOS/GRUB even start.\nType 'continue' (or 'c') to run to kernel_main.\n---\n
