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

- [x] Handle extended scancodes (arrows, function keys, etc.) — decoded as the ANSI/VT220 escape sequences a real terminal would send
- [x] Implement keyboard buffer/queue — ring buffer + read_byte/try_read_byte/has_data/read_line, mirrors `drivers::serial`'s input API
- [x] Add modifier key tracking (shift, ctrl, alt state) — `keyboard::modifiers()`; Ctrl+letter now produces real control codes (Ctrl+C = 0x03, etc.)

## Storage

- [x] ATA/AHCI driver for disk access — ATA PIO, primary bus/master drive only, polling (no AHCI/SATA)
- [x] Simple filesystem (FAT32 or custom) — FAT32 read/write, 8.3 names only, no subdirectory creation
- [x] initrd/ramdisk support — GRUB `module2` FAT32 image, mounted read-only from RAM (`make initrd`)

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
- [ ] Add `make debug` target for QEMU with GDB stub
- [ ] Consider UEFI boot support alongside legacy BIOS/multiboot2

## Code Quality

- [ ] Add unit tests (where possible in `no_std`)
- [ ] Document public APIs with rustdoc
- [ ] Set up CI (build + boot test)
