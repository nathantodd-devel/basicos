# basicos build system
#
# Targets:
#   make              - build debug ISO
#   make release      - build optimized, stripped release ISO
#   make run          - boot debug ISO in QEMU, with the test disk attached
#   make run-release  - boot release ISO in QEMU, with the test disk attached
#   make gdb          - boot debug ISO in QEMU, halted, waiting for GDB on :1234
#   make disk         - create build/disk.img (2G, FAT32) if it doesn't exist
#   make disk-reset   - wipe and recreate build/disk.img from scratch
#   make initrd       - create build/initrd.img (FAT32 initrd) if it doesn't exist
#   make initrd-reset - wipe and recreate build/initrd.img from scratch
#   make reset-disks  - disk-reset + initrd-reset
#   make userland     - build userland/*.c (see userland/Makefile)
#   make clean        - remove build artifacts
#   make size         - show kernel binary sizes
#
# Debugging with GDB: `make gdb` in one terminal, `gdb -x debug.gdb` in
# another — see debug.gdb's own header for what that sets up. The debug
# build keeps full DWARF info (only [profile.release] strips it), and
# -C force-frame-pointers=yes is set kernel-wide (kernel/.cargo/
# config.toml), so both GDB's `bt` and the kernel's own panic handler
# (`kernel/src/panic.rs`) produce real backtraces.
#
# Every optional subsystem (heap, paging, storage, keyboard, ...) is a
# Cargo feature, on by default. Build a reduced kernel with FEATURES:
#   make FEATURES="--no-default-features --features heap,keyboard"
# See [features] in kernel/Cargo.toml for the full list and what each
# gates.
#
# Workspace layout: kernel/ (the OS, a custom no_std target) and fs/
# (the FAT32 format, an ordinary host-target crate kernel/ depends on
# and a future host-side tool could too — see fs/src/lib.rs) are Cargo
# workspace members sharing the root Cargo.toml/Cargo.lock/target/. The
# cargo invocations below `cd kernel` first because kernel/.cargo/
# config.toml's settings (the custom target, build-std) only apply when
# Cargo is invoked with kernel/ as the working directory — config
# discovery walks up from cwd, not from --manifest-path.

ARCH        := x86_64
TARGET_SPEC := x86_64-basicos
BUILD_DIR   := build
ISO_DIR     := $(BUILD_DIR)/isofiles

AS          := as
LD          := ld
CARGO       := cargo
STRIP       := strip
OBJCOPY     := objcopy
QEMU        := qemu-system-x86_64
QEMU_IMG    := qemu-img
MKFS_VFAT   := mkfs.vfat
MCOPY       := mcopy

# Cargo feature flags, e.g. `--no-default-features --features heap`.
# Empty (the default) means "use Cargo.toml's default features" — i.e.
# everything.
FEATURES    :=

# Debug build paths
DEBUG_LIB   := target/$(TARGET_SPEC)/debug/libbasicos.a
DEBUG_BIN   := $(BUILD_DIR)/kernel.bin
DEBUG_ISO   := $(BUILD_DIR)/basicos.iso

# Release build paths
RELEASE_LIB := target/$(TARGET_SPEC)/release/libbasicos.a
RELEASE_BIN := $(BUILD_DIR)/kernel-release.bin
RELEASE_ISO := $(BUILD_DIR)/basicos-release.iso

ASM_OBJ     := $(BUILD_DIR)/entrypoint.o

# Find all Rust source files for dependency tracking — both workspace
# members, since fs/ changes should trigger a kernel rebuild too.
RUST_SOURCES := $(shell find kernel/src fs/src -name '*.rs')

# Disk images for the ATA/FAT32 storage stack.
# DISK_IMG is a virtual hard drive attached via `-drive` (what the ATA
# driver reads/writes); INITRD_IMG is a small FAT32 image loaded as a
# GRUB module and baked into the ISO (what mem::multiboot2 finds and
# fs::init_initrd mounts read-only from RAM). Both are "superfloppy"
# FAT32 — no partition table — since that's all our driver understands.
DISK_IMG    := $(BUILD_DIR)/disk.img
DISK_SIZE   := 2G
INITRD_IMG  := $(BUILD_DIR)/initrd.img
INITRD_SIZE := 8M

# Standalone test ELF binaries for the `elf-loader` feature. testprog.s
# is copied onto $(DISK_IMG) as /hello.elf; forktest.s (fork/exec/wait/
# mmap/brk, execve'ing into hello.elf on success) as /forktest.elf.
TESTPROG_OBJ := $(BUILD_DIR)/testprog.o
TESTPROG_ELF := $(BUILD_DIR)/testprog.elf
FORKTEST_OBJ := $(BUILD_DIR)/forktest.o
FORKTEST_ELF := $(BUILD_DIR)/forktest.elf

# userland/: the same idea, but built from C (see userland/syscalls.h
# and userland/Makefile) instead of hand-written assembly. Every
# userland/*.c program that builds gets copied onto $(DISK_IMG) as
# C:\testprogs\<name>.elf — under its own directory rather than
# alongside testprog/forktest's /hello.elf and /forktest.elf, since
# forkexec.elf execve's into the *assembly*-built /hello.elf on purpose
# (see its header comment) and the two shouldn't be mixed up.
USERLAND_DIR := userland

# -boot d: always boot the CD, regardless of the attached hard drive —
# without it QEMU can try the hard drive first once one is attached.
QEMU_FLAGS  := -m 256M -serial stdio -boot d -drive file=$(DISK_IMG),format=raw,if=ide,index=0

.PHONY: all debug release kernel-bin kernel-release iso iso-release run run-release gdb \
        disk disk-reset initrd initrd-reset reset-disks testprog forktest userland \
        clean size FORCE

all: iso

# Always-out-of-date prerequisite: forces the cargo-build rules below to
# actually invoke cargo every time, since file-mtime tracking alone
# wouldn't notice a `make FEATURES=...` override with no source changes
# (cargo's own fingerprinting is what makes repeat builds fast, not this).
FORCE:

debug: iso

release: iso-release

# ============================================================================
# Assembly
# ============================================================================

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

$(ASM_OBJ): kernel/entrypoint.s | $(BUILD_DIR)
	$(AS) --64 $< -o $@

# ============================================================================
# Debug Build
# ============================================================================

$(DEBUG_LIB): $(RUST_SOURCES) kernel/Cargo.toml fs/Cargo.toml kernel/$(TARGET_SPEC).json kernel/.cargo/config.toml FORCE
	cd kernel && $(CARGO) -Zjson-target-spec build $(FEATURES)

# Named kernel-bin, not kernel — a target literally named `kernel` would
# collide with the kernel/ directory (harmless in principle, since
# .PHONY targets bypass the filesystem check regardless of what's on
# disk with the same name, but confusing to read).
kernel-bin: $(ASM_OBJ) $(DEBUG_LIB) kernel/linker.ld
	$(LD) -n -T kernel/linker.ld -o $(DEBUG_BIN) $(ASM_OBJ) $(DEBUG_LIB)

iso: kernel-bin initrd
	mkdir -p $(ISO_DIR)/boot/grub
	cp $(DEBUG_BIN) $(ISO_DIR)/boot/kernel.bin
	cp $(INITRD_IMG) $(ISO_DIR)/boot/initrd.img
	cp kernel/grub.cfg $(ISO_DIR)/boot/grub/grub.cfg
	grub2-mkrescue -o $(DEBUG_ISO) $(ISO_DIR)

# ============================================================================
# Release Build (optimized + stripped)
# ============================================================================

$(RELEASE_LIB): $(RUST_SOURCES) kernel/Cargo.toml fs/Cargo.toml kernel/$(TARGET_SPEC).json kernel/.cargo/config.toml FORCE
	cd kernel && $(CARGO) -Zjson-target-spec build --release $(FEATURES)

kernel-release: $(ASM_OBJ) $(RELEASE_LIB) kernel/linker.ld
	$(LD) -n -T kernel/linker.ld -o $(RELEASE_BIN) $(ASM_OBJ) $(RELEASE_LIB)
	# Strip all symbols except those needed for multiboot
	$(STRIP) --strip-all $(RELEASE_BIN)
	# Alternative: keep only essential symbols
	# $(OBJCOPY) --strip-unneeded $(RELEASE_BIN)

iso-release: kernel-release initrd
	mkdir -p $(ISO_DIR)/boot/grub
	cp $(RELEASE_BIN) $(ISO_DIR)/boot/kernel.bin
	cp $(INITRD_IMG) $(ISO_DIR)/boot/initrd.img
	cp kernel/grub.cfg $(ISO_DIR)/boot/grub/grub.cfg
	grub2-mkrescue -o $(RELEASE_ISO) $(ISO_DIR)

# ============================================================================
# Disk images (FAT32) — the test hard drive + the initrd
# ============================================================================
#
# Both are built with mkfs.vfat directly on a plain file (no partition
# table, no loop devices, no root) and populated with mtools (mcopy),
# which also works straight against the image file. `make reset-disks`
# wipes and repopulates both from scratch — use it if a test run has
# left the disk in a state you don't want to keep debugging against.

$(DISK_IMG): $(TESTPROG_ELF) $(FORKTEST_ELF) userland
	mkdir -p $(BUILD_DIR)
	$(QEMU_IMG) create -f raw $(DISK_IMG) $(DISK_SIZE)
	$(MKFS_VFAT) -F 32 -n BASICOS $(DISK_IMG)
	echo "hello from the FAT32 test disk" | $(MCOPY) -i $(DISK_IMG) - ::/hello.txt
	mmd -i $(DISK_IMG) ::/docs
	echo "just some docs on the test disk" | $(MCOPY) -i $(DISK_IMG) - ::/docs/readme.txt
	$(MCOPY) -i $(DISK_IMG) $(TESTPROG_ELF) ::/hello.elf
	$(MCOPY) -i $(DISK_IMG) $(FORKTEST_ELF) ::/forktest.elf
	mmd -i $(DISK_IMG) ::/TESTPROG
	$(MCOPY) -i $(DISK_IMG) $(USERLAND_DIR)/build/*.elf ::/TESTPROG/

disk: $(DISK_IMG)

# testprog.s: a tiny standalone ELF64 executable (not part of the
# kernel) for exercising `elf::load`/`elf::run` — see its own header
# comment. Copied onto the test disk as /hello.elf above; try it from
# the shell with `run /hello.elf`.
$(TESTPROG_OBJ): testprog.s | $(BUILD_DIR)
	$(AS) --64 $< -o $@

$(TESTPROG_ELF): $(TESTPROG_OBJ) testprog.ld
	$(LD) -n -T testprog.ld -o $(TESTPROG_ELF) $(TESTPROG_OBJ)

testprog: $(TESTPROG_ELF)

# forktest.s: exercises fork/execve/wait4/mmap/brk — see its own header
# comment. Copied onto the test disk as /forktest.elf above; try it from
# the shell with `run /forktest.elf`.
$(FORKTEST_OBJ): forktest.s | $(BUILD_DIR)
	$(AS) --64 $< -o $@

$(FORKTEST_ELF): $(FORKTEST_OBJ) forktest.ld
	$(LD) -n -T forktest.ld -o $(FORKTEST_ELF) $(FORKTEST_OBJ)

forktest: $(FORKTEST_ELF)

# userland/: C programs against syscalls.h — see userland/Makefile for
# the actual build rules (compiler flags, crt0, linker script) and the
# full list of *.c files, which changes independently of this Makefile.
# Always delegated, same reasoning as the cargo-build rules above: let
# userland/Makefile's own dependency tracking decide what's stale.
.PHONY: userland
userland:
	$(MAKE) -C $(USERLAND_DIR)

disk-reset:
	rm -f $(DISK_IMG)
	$(MAKE) disk

$(INITRD_IMG):
	mkdir -p $(BUILD_DIR)
	$(QEMU_IMG) create -f raw $(INITRD_IMG) $(INITRD_SIZE)
	$(MKFS_VFAT) -F 32 -n INITRD $(INITRD_IMG)
	echo "hello from the initrd" | $(MCOPY) -i $(INITRD_IMG) - ::/hello.txt
	mmd -i $(INITRD_IMG) ::/etc
	echo "welcome to basicos" | $(MCOPY) -i $(INITRD_IMG) - ::/etc/motd.txt

initrd: $(INITRD_IMG)

initrd-reset:
	rm -f $(INITRD_IMG)
	$(MAKE) initrd

reset-disks: disk-reset initrd-reset

# ============================================================================
# Run
# ============================================================================

run: iso disk
	$(QEMU) -cdrom $(DEBUG_ISO) $(QEMU_FLAGS)

run-release: iso-release disk
	$(QEMU) -cdrom $(RELEASE_ISO) $(QEMU_FLAGS)

# -S: start halted (nothing runs — not even the BIOS/GRUB boot — until a
# debugger tells it to). -s: shorthand for -gdb tcp::1234. Always the
# *debug* ISO: [profile.release] strips symbols, so there'd be nothing
# for GDB to resolve against. See debug.gdb for the other half of this.
gdb: iso disk
	@echo "Waiting for a debugger on tcp::1234 (nothing runs until it attaches)."
	@echo "In another terminal: gdb -x debug.gdb"
	$(QEMU) -cdrom $(DEBUG_ISO) $(QEMU_FLAGS) -s -S

# ============================================================================
# Utilities
# ============================================================================

size: kernel-bin kernel-release
	@echo "=== Kernel Binary Sizes ==="
	@echo "Debug:"
	@ls -lh $(DEBUG_BIN) | awk '{print "  " $$5 " " $$9}'
	@size $(DEBUG_BIN) || true
	@echo ""
	@echo "Release (stripped):"
	@ls -lh $(RELEASE_BIN) | awk '{print "  " $$5 " " $$9}'
	@size $(RELEASE_BIN) || true
	@echo ""
	@echo "=== Rust Library Sizes ==="
	@ls -lh $(DEBUG_LIB) $(RELEASE_LIB) 2>/dev/null | awk '{print "  " $$5 " " $$9}'

clean:
	rm -rf $(BUILD_DIR) target
	$(MAKE) -C $(USERLAND_DIR) clean
