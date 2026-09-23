# userland/Makefile — builds every *.c program in this directory into a
# freestanding, statically-linked ET_EXEC ELF64 binary the kernel's own
# loader can run (see ../src/elf/mod.rs and syscalls.h's header comment
# for exactly what constraints that puts on the code here).
#
# Usage:
#   make            build every *.c here to build/<name>.elf
#   make clean
#
# The top-level Makefile drives this automatically (see its `userland`
# target) and copies the results onto the test disk image.

CC := gcc
LD := ld
AS := as

# -ffreestanding -nostdlib -nostdinc -static: no libc, no host runtime,
#   no host headers — this kernel provides its own syscall ABI
#   (syscalls.h), not Linux's, and crt0.s is the only startup code.
# -fno-pic -fno-pie: the kernel's ELF loader only understands ET_EXEC
#   with absolute addresses — no relocations, no PIE (see elf/mod.rs;
#   it rejects ET_DYN outright).
# -fno-stack-protector: __stack_chk_fail comes from libc, which isn't
#   linked in.
# -fcf-protection=none: skip CET/endbr64 codegen and the
#   .note.gnu.property section modern distro gcc emits by default —
#   nothing in this kernel understands (or needs) it, and it's one less
#   thing for user.ld's /DISCARD/ to have to catch.
# -mno-red-zone: defensive, not strictly load-bearing here (any
#   interrupt/exception switches off the user stack via TSS.RSP0 before
#   touching kernel memory — see gdt.rs's `jump_to_ring3*`), but it's
#   the standard, expected default for freestanding/kernel-style code.
# -mgeneral-regs-only: IMPORTANT, not just style — see syscalls.h's
#   header comment. The scheduler's context switch (sched::switch())
#   only saves the general-purpose callee-saved registers; it never
#   touches XMM/x87/MMX state. Ordinary C code on x86_64 reaches for SSE
#   registers by default (gcc will use them for routine struct copies
#   even with no explicit floating point in sight), and without this
#   flag a timer tick preempting mid-use would silently corrupt it —
#   this flag makes the compiler never emit any of those instructions
#   in the first place, so there's no state to lose.
CFLAGS := -ffreestanding -nostdlib -nostdinc -static \
          -fno-pic -fno-pie -fno-stack-protector -fcf-protection=none \
          -mno-red-zone -mgeneral-regs-only \
          -Wall -Wextra -O2 -c

BUILD_DIR := build
SOURCES   := $(wildcard *.c)
TARGETS   := $(patsubst %.c,$(BUILD_DIR)/%.elf,$(SOURCES))
CRT0_OBJ  := $(BUILD_DIR)/crt0.o

.PHONY: all clean
all: $(TARGETS)

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

$(CRT0_OBJ): crt0.s | $(BUILD_DIR)
	$(AS) --64 $< -o $@

$(BUILD_DIR)/%.o: %.c syscalls.h | $(BUILD_DIR)
	$(CC) $(CFLAGS) $< -o $@

# -n: no page alignment padding between segments (matches the rest of
# this project's linker invocations — ld's default page-aligned output
# is unnecessary bloat for a binary this small).
$(BUILD_DIR)/%.elf: $(BUILD_DIR)/%.o $(CRT0_OBJ) user.ld
	$(LD) -n -T user.ld -o $@ $(CRT0_OBJ) $<

clean:
	rm -rf $(BUILD_DIR)
