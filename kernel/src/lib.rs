//! basicos — a tiny x86_64 kernel.
//!
//! Called from `entrypoint.s` after long mode + paging is set up.
//! `kernel_main` is the Rust-side entry point; it never returns.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

#[cfg(feature = "heap")]
extern crate alloc;

mod arch;
mod drivers;
#[cfg(feature = "elf-loader")]
mod elf;
#[cfg(feature = "storage")]
mod fs;
mod log;
mod mem;
#[cfg(feature = "net")]
mod net;
mod panic;
#[cfg(feature = "scheduler")]
mod sched;
#[cfg(feature = "shell")]
mod shell;
mod sync;
#[cfg(feature = "usermode-demo")]
mod syscall;
#[cfg(feature = "usermode-demo")]
mod usermode;

// Re-export commonly used items
pub use log::{LogLevel, RateLimiter};
#[cfg(feature = "usermode-demo")]
pub use syscall::nr as syscall_nr;

// Convenient aliases for internal use
#[cfg(feature = "usermode-demo")]
use arch::x86_64::syscall as arch_syscall;
use arch::{cpu_info, gdt, interrupts};
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
        panic!("Invalid multiboot2 magic: {:#x}", magic);
        // loop {
        //     unsafe { core::arch::asm!("cli", "hlt") };
        // }
    }

    // Parse multiboot2 memory map and initialize frame allocator
    let memory_map = unsafe { mem::multiboot2::parse_memory_map(mb_info_addr) };
    pr_info!(
        "Memory map: {} regions, {} MiB available",
        memory_map.len(),
        memory_map.total_available() / (1024 * 1024)
    );

    // Initialize the physical frame allocator
    #[cfg(feature = "frame-alloc")]
    {
        mem::frame::init(&memory_map);
        pr_info!(
            "Frame allocator: {}/{} frames free ({} MiB)",
            mem::frame::free_count(),
            mem::frame::total_frames(),
            mem::frame::free_count() * 4 / 1024
        );
    }

    // Look for a bootloader module (GRUB `module2` — our initrd) and
    // reserve its frames *before* paging or anything else gets a chance
    // to hand them out to something else. We don't read its contents
    // yet; that happens later, once the heap is up.
    #[cfg(feature = "storage")]
    let initrd_module = unsafe { mem::multiboot2::find_first_module(mb_info_addr) };
    #[cfg(feature = "storage")]
    if let Some(module) = &initrd_module {
        mem::frame::reserve_range(module.start as usize, module.end as usize);
    }

    // Same idea for the fixed-address process zone: reserve it before
    // the heap (below) claims frames dynamically, so the two can never
    // overlap, and before `demote_process_zone` below hands out frames
    // for its own page tables. See `mem::paging`'s module docs on
    // `PROC_ZONE_START` for why this range is special.
    #[cfg(feature = "elf-loader")]
    mem::frame::reserve_range(mem::paging::PROC_ZONE_START, mem::paging::PROC_ZONE_END);

    // Initialize paging system
    #[cfg(feature = "paging")]
    {
        mem::paging::init();
        pr_info!("Paging initialized");
    }

    // Turn the process zone's huge-page mapping into real 4 KiB page
    // tables, so each process (`sched::spawn_process`/`fork`) can get its
    // own private mapping there. Must come after `paging::init()` (needs
    // the kernel's own page tables) and the reservation above (needs
    // `frame::allocate()` to never hand out a frame from inside the zone
    // for its own bookkeeping).
    #[cfg(feature = "elf-loader")]
    {
        mem::paging::demote_process_zone();
        mem::paging::protect_kernel_mappings();
    }

    unsafe { gdt::init() };
    pr_info!("GDT + TSS loaded");

    #[cfg(feature = "heap")]
    {
        let heap_size = mem::heap::init();
        pr_info!(
            "Heap allocator initialized ({} MiB)",
            heap_size / (1024 * 1024)
        );
    }

    // Mount storage — three DOS-style drives, all FAT32 (see `fs`):
    // A: a fresh writable ramdisk, B: the initrd (if GRUB loaded one),
    // C: the primary ATA disk (if present).
    #[cfg(feature = "storage")]
    {
        match fs::init_ramdisk() {
            Ok(()) => pr_info!(
                "A: formatted + mounted a {} KiB FAT32 ramdisk",
                basicos_fs::ramdisk::TOTAL_SECTORS * fs::BLOCK_SIZE / 1024
            ),
            Err(e) => pr_warn!("A: {}", e),
        }
        match initrd_module {
            Some(module) => {
                // Safety: reserved in the frame allocator above, before
                // paging or anything else could have claimed these frames.
                let bytes = unsafe { module.as_slice() };
                match fs::init_initrd(bytes) {
                    Ok(()) => {
                        pr_info!("B: mounted FAT32 initrd image ({} KiB)", bytes.len() / 1024)
                    }
                    Err(e) => pr_warn!("B: {}", e),
                }
            }
            None => pr_info!("B: no initrd module supplied by bootloader"),
        }
        // Every ATA disk, and every partition on it, that holds a FAT32
        // or ext4 filesystem gets the next drive letter from C:.
        let mut mounted = 0;
        for r in fs::mount_disks() {
            match r {
                Ok(m) => {
                    mounted += 1;
                    pr_info!(
                        "{}: mounted {} {}{} ({} MiB, {}, {}){}",
                        m.drive.letter(),
                        m.fs_type,
                        if m.label.is_empty() { "volume" } else { "volume " },
                        m.label,
                        m.bytes >> 20,
                        m.source,
                        if m.read_only { "read-only" } else { "read-write" },
                        m.note.map(|n| alloc::format!(" — {}", n)).unwrap_or_default()
                    )
                }
                Err(why) => pr_warn!("disk: {}", why),
            }
        }
        if mounted == 0 {
            pr_warn!("no disk volumes mounted (no ATA disk with a FAT32/ext4 filesystem found)");
        }
    }

    #[cfg(feature = "usermode-demo")]
    {
        unsafe { arch_syscall::init() };
        pr_info!("SYSCALL/SYSRET initialized");
    }

    interrupts::init();
    pr_info!("Interrupts enabled");

    // Display CPU info
    let vendor = cpu_info::get_cpu_vendor();
    let brand = cpu_info::get_cpu_brand();
    pr_info!("CPU: {} - {}", vendor.as_str(), brand.as_str());

    // Boot tail. Exactly one of these arms diverges, whatever the feature
    // set (see `make feature-check`):
    //   scheduler on        -> spawn what's enabled, then `sched::run()`
    //   usermode-demo only  -> one-way jump to the ring-3 demo
    //   neither             -> halt
    #[cfg(feature = "scheduler")]
    {
        // Networking: find a NIC and spawn the `net` task (must come
        // before `sched::run()` below, which never returns).
        #[cfg(feature = "net")]
        net::init();

        // The first user process: C:/INIT.ELF. Everything else in
        // userspace, the shell included, is started by it.
        #[cfg(feature = "elf-loader")]
        {
            let _pid = elf::spawn("init", "C:/INIT.ELF").expect("INIT.ELF required for boot");
        }

        // The ring-0 debug monitor (off by default — see `shell`).
        #[cfg(feature = "shell")]
        sched::spawn("shell", shell::shell_task);

        // Without init or a shell there'd be nothing to look at — run the
        // two preemption demo tasks instead.
        #[cfg(not(any(feature = "elf-loader", feature = "shell")))]
        {
            sched::spawn("demo-a", sched::demo_task_a);
            sched::spawn("demo-b", sched::demo_task_b);
        }

        sched::run();
    }

    #[cfg(all(not(feature = "scheduler"), feature = "usermode-demo"))]
    {
        pr_notice!("Jumping to ring 3...");
        let stack_top =
            unsafe { core::ptr::addr_of!(USER_STACK.0) as u64 + USER_STACK_SIZE as u64 };
        unsafe { gdt::jump_to_ring3(usermode::user_mode_stub, stack_top) }
    }

    #[cfg(all(not(feature = "scheduler"), not(feature = "usermode-demo")))]
    {
        pr_notice!("Boot sequence complete (usermode-demo disabled) — halting.");
        loop {
            unsafe { core::arch::asm!("cli", "hlt") };
        }
    }
}

#[cfg(feature = "usermode-demo")]
const USER_STACK_SIZE: usize = 4 * 1024; // 4 KiB user stack

#[cfg(feature = "usermode-demo")]
#[repr(align(16))]
struct UserStack([u8; USER_STACK_SIZE]);

#[cfg(feature = "usermode-demo")]
static mut USER_STACK: UserStack = UserStack([0; USER_STACK_SIZE]);
