//! A small interactive command shell. Runs as a `sched` task (that's
//! what the `shell` feature implies `scheduler` for) — it's ring-0, not
//! a ring-3 process, so it can `ls`/`cat` and call into the kernel
//! directly rather than through a syscall.
//!
//! Reads from serial if `serial-input` is on (also the only practical
//! way to drive it from an automated test — piping text into QEMU's
//! `-serial stdio`), otherwise the keyboard if `keyboard` is on,
//! otherwise it just says so and idles.

use crate::{kprint, kprintln};

const LINE_BUF_SIZE: usize = 128;

/// Block for one line of input. Returns the number of bytes read.
fn read_line(buf: &mut [u8]) -> usize {
    #[cfg(feature = "serial-input")]
    {
        return crate::drivers::serial::read_line(buf);
    }
    #[cfg(all(not(feature = "serial-input"), feature = "keyboard"))]
    {
        return crate::drivers::keyboard::read_line(buf);
    }
    #[cfg(all(not(feature = "serial-input"), not(feature = "keyboard")))]
    {
        let _ = buf;
        kprintln!("shell: no input driver compiled in (need `serial-input` or `keyboard`) — idling");
        loop {
            crate::sched::yield_now();
        }
    }
}

fn cmd_help() {
    kprintln!("commands:");
    kprintln!("  help              this message");
    kprintln!("  echo <text>       print text back");
    kprintln!("  clear             clear the VGA screen");
    kprintln!("  uptime            time since boot");
    kprintln!("  ps                list scheduler tasks");
    kprintln!("  meminfo           frame allocator stats");
    kprintln!("  drives            list mounted drives");
    kprintln!("  ls [drive:][path] list a directory (default C:, e.g. 'ls A:')");
    kprintln!("  cat <path>        print a file");
    kprintln!("  write <path> text create/overwrite a file with text");
    kprintln!("  run <path>        load an ELF binary as a new process (runs in the background)");
    kprintln!("  heapfill          spawn a task that appends to a Vec until the heap is full");
    kprintln!("drives: A: ramdisk (writable), B: initrd (read-only), C: main disk");
}

fn cmd_uptime() {
    use crate::arch::interrupts::{TIMER_HZ, TIMER_TICKS};
    let ticks = TIMER_TICKS.load(core::sync::atomic::Ordering::Relaxed);
    let hz = TIMER_HZ as u64;
    kprintln!(
        "uptime: {}.{:02}s ({} ticks @ {} Hz)",
        ticks / hz,
        (ticks % hz) * 100 / hz,
        ticks,
        hz
    );
}

fn cmd_ps() {
    kprintln!("  NAME          STATE");
    crate::sched::for_each_task(|name, state| {
        kprintln!("  {:<12}  {:?}", name, state);
    });
}

#[cfg(feature = "frame-alloc")]
fn cmd_meminfo() {
    let free = crate::mem::frame::free_count();
    let total = crate::mem::frame::total_frames();
    kprintln!(
        "frames: {}/{} free ({} KiB / {} KiB)",
        free,
        total,
        free * 4,
        total * 4
    );
}
#[cfg(not(feature = "frame-alloc"))]
fn cmd_meminfo() {
    kprintln!("meminfo: `frame-alloc` feature not enabled");
}

#[cfg(feature = "storage")]
fn cmd_drives() {
    for drive in [
        crate::fs::Drive::Ramdisk,
        crate::fs::Drive::Initrd,
        crate::fs::Drive::Disk,
    ] {
        let mounted = crate::fs::with_drive(drive, |_| ()).is_some();
        kprintln!(
            "  {}:  {}",
            drive.letter(),
            if mounted { "mounted" } else { "not mounted" }
        );
    }
}
#[cfg(not(feature = "storage"))]
fn cmd_drives() {
    kprintln!("drives: `storage` feature not enabled");
}

#[cfg(feature = "storage")]
fn cmd_ls(path: &str) {
    let (drive, rest) = crate::fs::resolve_path(path);
    let rest = if rest.is_empty() { "/" } else { rest };
    match crate::fs::with_drive(drive, |vol| vol.read_dir_at(rest)) {
        Some(Ok(entries)) => {
            for e in &entries {
                kprintln!("  {}{}", e.name, if e.is_dir { "/" } else { "" });
            }
        }
        Some(Err(e)) => kprintln!("ls: {:?}", e),
        None => kprintln!("ls: {}: not mounted", drive.letter()),
    }
}
#[cfg(not(feature = "storage"))]
fn cmd_ls(_path: &str) {
    kprintln!("ls: `storage` feature not enabled");
}

#[cfg(feature = "storage")]
fn cmd_cat(path: &str) {
    if path.is_empty() {
        kprintln!("usage: cat <path>");
        return;
    }
    let (drive, rest) = crate::fs::resolve_path(path);
    match crate::fs::with_drive(drive, |vol| vol.read_file_at(rest)) {
        Some(Ok(data)) => match core::str::from_utf8(&data) {
            Ok(s) => kprintln!("{}", s),
            Err(_) => kprintln!("cat: {} bytes, not valid UTF-8", data.len()),
        },
        Some(Err(e)) => kprintln!("cat: {:?}", e),
        None => kprintln!("cat: {}: not mounted", drive.letter()),
    }
}
#[cfg(not(feature = "storage"))]
fn cmd_cat(_path: &str) {
    kprintln!("cat: `storage` feature not enabled");
}

#[cfg(feature = "storage")]
fn cmd_write(arg: &str) {
    let (path, text) = match arg.split_once(' ') {
        Some((p, t)) => (p, t),
        None => {
            kprintln!("usage: write <path> <text>");
            return;
        }
    };
    let (drive, rest) = crate::fs::resolve_path(path);
    match crate::fs::with_drive(drive, |vol| vol.write_file(rest, text.as_bytes())) {
        Some(Ok(())) => kprintln!("wrote {} bytes to {}", text.len(), path),
        Some(Err(e)) => kprintln!("write: {:?}", e),
        None => kprintln!("write: {}: not mounted", drive.letter()),
    }
}
#[cfg(not(feature = "storage"))]
fn cmd_write(_arg: &str) {
    kprintln!("write: `storage` feature not enabled");
}

/// Appends to a `Vec<u8>` forever, as its own background task, until the
/// heap is exhausted. Deliberately *not* something the shell command
/// runs inline: as a separate task it doesn't block the shell prompt
/// (watch it eat memory with `meminfo`/`ps` while it runs), and — the
/// point of putting this here at all — it runs at genuine ring 0, where
/// the heap's `cli`-based `Spinlock` is actually legal to use. See the
/// comment at the end of `usermode::user_mode_stub` for what happens if
/// the same loop runs at ring 3 instead (a confusing #GP, not this).
///
/// Ending in a panic is the point, not a bug to fix: once
/// `GlobalAlloc::alloc` returns null, Rust's normal `handle_alloc_error`
/// path takes over, which panics — a real, diagnosable "memory
/// allocation of N bytes failed" through `panic.rs` (VGA + serial +
/// stack trace), rather than the heap silently refusing to grow further.
#[cfg(feature = "heap")]
extern "C" fn heapfill_task() -> ! {
    use alloc::vec::Vec;

    kprintln!("[heapfill] appending to a Vec<u8> until the heap is exhausted...");
    let progress_step = 8 * 1024 * 1024; // log every 8 MiB
    let mut next_log = progress_step;
    let mut v: Vec<u8> = Vec::new();
    loop {
        v.push(0);
        if v.len() >= next_log {
            kprintln!("[heapfill] {} MiB appended so far", v.len() / (1024 * 1024));
            next_log += progress_step;
        }
    }
}

#[cfg(feature = "heap")]
fn cmd_heapfill() {
    match crate::sched::spawn("heapfill", heapfill_task) {
        Some(_) => kprintln!(
            "heapfill: spawned — runs in the background until the heap is exhausted, then panics"
        ),
        None => kprintln!("heapfill: scheduler task table is full"),
    }
}
#[cfg(not(feature = "heap"))]
fn cmd_heapfill() {
    kprintln!("heapfill: `heap` feature not enabled");
}

#[cfg(feature = "elf-loader")]
fn cmd_run(path: &str) {
    if path.is_empty() {
        kprintln!("usage: run <path>");
        return;
    }
    match crate::elf::spawn("run", path) {
        Ok(pid) => kprintln!("run: {} -> pid {} (runs in the background)", path, pid),
        Err(e) => kprintln!("run: failed: {:?}", e),
    }
}
#[cfg(not(feature = "elf-loader"))]
fn cmd_run(_path: &str) {
    kprintln!("run: `elf-loader` feature not enabled");
}

fn dispatch(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (line, ""),
    };
    match cmd {
        "help" => cmd_help(),
        "echo" => kprintln!("{}", arg),
        "clear" => crate::drivers::vga::clear_screen(),
        "uptime" => cmd_uptime(),
        "ps" => cmd_ps(),
        "meminfo" => cmd_meminfo(),
        "drives" => cmd_drives(),
        "ls" => cmd_ls(arg),
        "cat" => cmd_cat(arg),
        "write" => cmd_write(arg),
        "run" => cmd_run(arg),
        "heapfill" => cmd_heapfill(),
        _ => kprintln!("unknown command: {} (try 'help')", cmd),
    }
}

pub extern "C" fn shell_task() -> ! {
    kprintln!();
    kprintln!("basicos shell — type 'help' for commands");
    let mut buf = [0u8; LINE_BUF_SIZE];
    loop {
        kprint!("basicos> ");
        let n = read_line(&mut buf).min(LINE_BUF_SIZE);
        let line = core::str::from_utf8(&buf[..n]).unwrap_or("");
        dispatch(line);
        crate::sched::yield_now();
    }
}
