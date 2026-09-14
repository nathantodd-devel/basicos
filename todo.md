# basicos TODO

## Logging System Enhancements

- [x] Reprogram PIT to 100Hz for accurate timestamps (currently using default ~18.2Hz)
- [ ] Add `pr_cont!` macro for continuation lines (like Linux's `KERN_CONT`)
- [ ] Implement `dmesg` command to dump ring buffer contents
- [ ] Add log level filtering at compile time (strip debug logs from release builds)
- [ ] Consider adding structured logging (key=value pairs)

## Memory Management

- [x] Implement physical frame allocator — bitmap-based, tracks 4 KiB frames up to 1 GiB
- [x] Implement heap allocator (`#[global_allocator]`) — fixed-size `.bss` region + free-list allocator
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
- [ ] Add process/task abstraction
- [ ] Implement scheduler (round-robin to start)
- [ ] ELF loader for userspace binaries
- [ ] Basic shell

## VGA / Display

- [ ] Add color support to VGA driver (currently hardcoded light grey on black)
- [ ] Implement cursor blinking
- [ ] Consider framebuffer mode for graphics (VESA/GOP)

## Keyboard

- [ ] Handle extended scancodes (arrows, function keys, etc.)
- [ ] Implement keyboard buffer/queue
- [ ] Add modifier key tracking (shift, ctrl, alt state)

## Storage

- [x] ATA/AHCI driver for disk access — ATA PIO, primary bus/master drive only, polling (no AHCI/SATA)
- [x] Simple filesystem (FAT32 or custom) — FAT32 read/write, 8.3 names only, no subdirectory creation
- [x] initrd/ramdisk support — GRUB `module2` FAT32 image, mounted read-only from RAM (`make initrd`)

## Build System

- [x] Fix linker warning about RWX segment permissions
- [x] Add release build profile with optimizations
- [ ] Add `make debug` target for QEMU with GDB stub
- [ ] Consider UEFI boot support alongside legacy BIOS/multiboot2

## Code Quality

- [ ] Add unit tests (where possible in `no_std`)
- [ ] Document public APIs with rustdoc
- [ ] Set up CI (build + boot test)
