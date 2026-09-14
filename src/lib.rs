//! basicos — a tiny x86_64 kernel.
//!
//! Called from `entrypoint.s` after long mode + paging is set up.
//! `kernel_main` is the Rust-side entry point; it never returns.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;

mod arch;
mod drivers;
mod fs;
mod log;
mod mem;
mod sync;
mod syscall;
mod usermode;

// Re-export commonly used items
pub use log::{LogLevel, RateLimiter};
pub use syscall::nr as syscall_nr;

// Convenient aliases for internal use
use arch::{cpu_info, gdt, interrupts, x86_64::syscall as arch_syscall};
use drivers::{serial, vga};

/// Entry point called from assembly.
///
/// # Arguments
/// * `magic` - multiboot2 magic value (0x36d76289), passed in RDI
/// * `mb_info_addr` - physical address of the multiboot2 info struct, RSI
#[no_mangle]
pub extern "C" fn kernel_main(magic: u64, mb_info_addr: u64) -> ! {
    vga::clear_screen();

    // Initialize serial early for debug output
    unsafe { serial::init() };

    pr_info!("basicos booted!");
    pr_info!("Multiboot2 + long mode transition succeeded");

    // Verify multiboot2 magic
    if magic as u32 != mem::multiboot2::MULTIBOOT2_BOOTLOADER_MAGIC {
        pr_emerg!("Invalid multiboot2 magic: {:#x}", magic);
        loop {
            unsafe { core::arch::asm!("cli", "hlt") };
        }
    }

    // Parse multiboot2 memory map and initialize frame allocator
    let memory_map = unsafe { mem::multiboot2::parse_memory_map(mb_info_addr) };
    pr_info!(
        "Memory map: {} regions, {} MiB available",
        memory_map.len(),
        memory_map.total_available() / (1024 * 1024)
    );

    // Initialize frame allocator
    mem::frame::init(&memory_map);
    pr_info!(
        "Frame allocator: {}/{} frames free ({} MiB)",
        mem::frame::free_count(),
        mem::frame::total_frames(),
        mem::frame::free_count() * 4 / 1024
    );

    // Look for a bootloader module (GRUB `module2` — our initrd) and
    // reserve its frames *before* paging or anything else gets a chance
    // to hand them out to something else. We don't read its contents
    // yet; that happens later, once the heap is up.
    let initrd_module = unsafe { mem::multiboot2::find_first_module(mb_info_addr) };
    if let Some(module) = &initrd_module {
        mem::frame::reserve_range(module.start as usize, module.end as usize);
    }

    // Initialize paging system
    mem::paging::init();
    pr_info!("Paging initialized");

    unsafe { gdt::init() };
    pr_info!("GDT + TSS loaded");

    mem::heap::init();
    pr_info!(
        "Heap allocator initialized ({} KiB)",
        mem::heap::HEAP_SIZE / 1024
    );

    // Mount storage: the primary ATA disk (if present) and the initrd
    // (if GRUB loaded one). Both are FAT32 — see `fs::fat32`.
    match fs::init_disk() {
        Ok(()) => pr_info!("Disk: mounted FAT32 volume on primary ATA drive"),
        Err(e) => pr_warn!("Disk: {}", e),
    }
    match initrd_module {
        Some(module) => {
            // Safety: reserved in the frame allocator above, before
            // paging or anything else could have claimed these frames.
            let bytes = unsafe { module.as_slice() };
            match fs::init_initrd(bytes) {
                Ok(()) => pr_info!("initrd: mounted FAT32 image ({} KiB)", bytes.len() / 1024),
                Err(e) => pr_warn!("initrd: {}", e),
            }
        }
        None => pr_info!("initrd: no module supplied by bootloader"),
    }

    unsafe { arch_syscall::init() };
    pr_info!("SYSCALL/SYSRET initialized");

    interrupts::init();
    pr_info!("Interrupts enabled");

    // Display CPU info
    let vendor = cpu_info::get_cpu_vendor();
    let brand = cpu_info::get_cpu_brand();
    pr_info!("CPU: {} - {}", vendor.as_str(), brand.as_str());

    pr_notice!("Jumping to ring 3...");
    let stack_top = unsafe { core::ptr::addr_of!(USER_STACK.0) as u64 + USER_STACK_SIZE as u64 };
    unsafe { gdt::jump_to_ring3(usermode::user_mode_stub, stack_top) }
}

const USER_STACK_SIZE: usize = 4 * 1024; // 4 KiB user stack

#[repr(align(16))]
struct UserStack([u8; USER_STACK_SIZE]);

static mut USER_STACK: UserStack = UserStack([0; USER_STACK_SIZE]);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    pr_emerg!("KERNEL PANIC!");
    if let Some(location) = info.location() {
        pr_emerg!(
            "  at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }
    pr_emerg!("  {}", info.message());

    loop {
        unsafe { core::arch::asm!("cli", "hlt") };
    }
}
