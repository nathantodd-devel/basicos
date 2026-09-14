# basicos build system
#
# Targets:
#   make              - build debug ISO
#   make release      - build optimized, stripped release ISO
#   make run          - boot debug ISO in QEMU, with the test disk attached
#   make run-release  - boot release ISO in QEMU, with the test disk attached
#   make disk         - create build/disk.img (2G, FAT32) if it doesn't exist
#   make disk-reset   - wipe and recreate build/disk.img from scratch
#   make initrd       - create build/initrd.img (FAT32 initrd) if it doesn't exist
#   make initrd-reset - wipe and recreate build/initrd.img from scratch
#   make reset-disks  - disk-reset + initrd-reset
#   make clean        - remove build artifacts
#   make size         - show kernel binary sizes
#
# Every optional subsystem (heap, paging, storage, keyboard, ...) is a
# Cargo feature, on by default. Build a reduced kernel with FEATURES:
#   make FEATURES="--no-default-features --features heap,keyboard"
# See [features] in Cargo.toml for the full list and what each gates.

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

# Find all Rust source files for dependency tracking
RUST_SOURCES := $(shell find src -name '*.rs')

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

# -boot d: always boot the CD, regardless of the attached hard drive —
# without it QEMU can try the hard drive first once one is attached.
QEMU_FLAGS  := -m 256M -serial stdio -boot d -drive file=$(DISK_IMG),format=raw,if=ide,index=0

.PHONY: all debug release kernel kernel-release iso iso-release run run-release \
        disk disk-reset initrd initrd-reset reset-disks clean size FORCE

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

$(ASM_OBJ): entrypoint.s | $(BUILD_DIR)
	$(AS) --64 $< -o $@

# ============================================================================
# Debug Build
# ============================================================================

$(DEBUG_LIB): $(RUST_SOURCES) Cargo.toml $(TARGET_SPEC).json .cargo/config.toml FORCE
	$(CARGO) -Zjson-target-spec build $(FEATURES)

kernel: $(ASM_OBJ) $(DEBUG_LIB) linker.ld
	$(LD) -n -T linker.ld -o $(DEBUG_BIN) $(ASM_OBJ) $(DEBUG_LIB)

iso: kernel initrd
	mkdir -p $(ISO_DIR)/boot/grub
	cp $(DEBUG_BIN) $(ISO_DIR)/boot/kernel.bin
	cp $(INITRD_IMG) $(ISO_DIR)/boot/initrd.img
	cp grub.cfg $(ISO_DIR)/boot/grub/grub.cfg
	grub2-mkrescue -o $(DEBUG_ISO) $(ISO_DIR)

# ============================================================================
# Release Build (optimized + stripped)
# ============================================================================

$(RELEASE_LIB): $(RUST_SOURCES) Cargo.toml $(TARGET_SPEC).json .cargo/config.toml FORCE
	$(CARGO) -Zjson-target-spec build --release $(FEATURES)

kernel-release: $(ASM_OBJ) $(RELEASE_LIB) linker.ld
	$(LD) -n -T linker.ld -o $(RELEASE_BIN) $(ASM_OBJ) $(RELEASE_LIB)
	# Strip all symbols except those needed for multiboot
	$(STRIP) --strip-all $(RELEASE_BIN)
	# Alternative: keep only essential symbols
	# $(OBJCOPY) --strip-unneeded $(RELEASE_BIN)

iso-release: kernel-release initrd
	mkdir -p $(ISO_DIR)/boot/grub
	cp $(RELEASE_BIN) $(ISO_DIR)/boot/kernel.bin
	cp $(INITRD_IMG) $(ISO_DIR)/boot/initrd.img
	cp grub.cfg $(ISO_DIR)/boot/grub/grub.cfg
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

$(DISK_IMG):
	mkdir -p $(BUILD_DIR)
	$(QEMU_IMG) create -f raw $(DISK_IMG) $(DISK_SIZE)
	$(MKFS_VFAT) -F 32 -n BASICOS $(DISK_IMG)
	echo "hello from the FAT32 test disk" | $(MCOPY) -i $(DISK_IMG) - ::/hello.txt
	mmd -i $(DISK_IMG) ::/docs
	echo "just some docs on the test disk" | $(MCOPY) -i $(DISK_IMG) - ::/docs/readme.txt

disk: $(DISK_IMG)

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

# ============================================================================
# Utilities
# ============================================================================

size: kernel kernel-release
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
