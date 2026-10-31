//! A ring-0 debug monitor. Runs as a `sched` task (that's
//! what the `shell` feature implies `scheduler` for) — it's ring-0, not
//! a ring-3 process, so it can `ls`/`cat` and call into the kernel
//! directly rather than through a syscall.
//!
//! Reads from serial if `serial-input` is on (also the only practical
//! way to drive it from an automated test — piping text into QEMU's
//! `-serial stdio`), otherwise the keyboard if `keyboard` is on,
//! otherwise it just says so and idles.
//!
//! This is a debug monitor, off by default: the shell people actually use
//! is `programs/bash`, which init starts. Turn this on (`make FEATURES=
//! "--features shell"`) to poke at kernel state directly, bearing in mind
//! that both shells then read the one console, so each typed line goes to
//! whichever gets there first (see `syscall::console`).

use crate::{kprint, kprintln};

// Must be at least `syscall::console::MAX_LINE` when `usermode-demo` is
// on: `console::read` hands out a completed line whole or not at all
// (never torn to fit — see that module's doc comment), so a buffer any
// smaller would just never receive a line that hit the limit.
const LINE_BUF_SIZE: usize = 256;

/// Block for one line of input. Returns the number of bytes read (not
/// including the newline).
fn read_line(buf: &mut [u8]) -> usize {
    #[cfg(feature = "usermode-demo")]
    {
        // One call, one whole line: `console::read` never splits a
        // completed line across calls (see its doc comment) — reading
        // it a byte at a time here would just reopen that door, since a
        // timer tick between two of those calls can hand the next byte
        // to a *different* blocked reader (a ring-3 process's `read(0,
        // ..)` goes through the very same queue).
        let n = crate::syscall::console::read(buf, false).max(0) as usize;
        return if n > 0 && buf[n - 1] == b'\n' { n - 1 } else { n };
    }
    #[cfg(not(feature = "usermode-demo"))]
    {
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
    kprintln!("  run <path> [args] load an ELF binary as a new process (runs in the background)");
    kprintln!("  heapfill          spawn a task that appends to a Vec until the heap is full");
    #[cfg(feature = "storage")]
    {
        kprintln!("  mkdir <path>      create a directory");
        kprintln!("  rmdir <path>      remove an empty directory");
        kprintln!("  rm <path>         delete a file");
        kprintln!("  mv <from> <to>    move/rename (within one drive)");
    }
    #[cfg(feature = "elf-loader")]
    kprintln!("  kill <pid>        terminate a process");
    #[cfg(feature = "usermode-demo")]
    kprintln!("  date              wall-clock time (UTC, from the RTC)");
    #[cfg(feature = "net")]
    {
        kprintln!("  ifconfig          network interface address and packet counters");
        kprintln!("  arp               the ARP cache");
        kprintln!("  ping <ip> [n]     send n (default 4) ICMP echo requests");
    }
    kprintln!("drives: A: ramdisk, B: initrd (read-only), C: and up: disk volumes (FAT32/ext4) — see `drives`");
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
    kprintln!("  DRIVE  TYPE   SIZE      FREE      MODE  SOURCE / LABEL");
    let mib = |b: u64| alloc::format!("{} MiB", b >> 20);
    for d in crate::fs::drives() {
        kprintln!(
            "  {}:     {:<6} {:<9} {:<9} {:<5} {}{}{}",
            d.drive.letter(),
            d.fs_type,
            mib(d.total),
            if d.free == 0 && d.total == 0 { alloc::string::String::from("?") } else { mib(d.free) },
            if d.read_only { "ro" } else { "rw" },
            d.source,
            if d.label.is_empty() { "" } else { "  " },
            d.label
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

/// `mkdir`/`rmdir`/`rm`/`mv`: one-liners over `basicos_fs`.
#[cfg(feature = "storage")]
fn fs_op(name: &str, path: &str, f: impl FnOnce(&mut dyn crate::fs::FileSystem, &str) -> Result<(), crate::fs::FsError>) {
    if path.is_empty() {
        kprintln!("usage: {} <path>", name);
        return;
    }
    let (drive, rest) = crate::fs::resolve_path(path);
    if drive == crate::fs::Drive::Initrd || crate::fs::with_drive(drive, |v| v.read_only()) == Some(true) {
        kprintln!("{}: {}: is read-only", name, drive.letter());
        return;
    }
    match crate::fs::with_drive(drive, |vol| f(vol, rest)) {
        Some(Ok(())) => {}
        Some(Err(e)) => kprintln!("{}: {}: {:?}", name, path, e),
        None => kprintln!("{}: {}: not mounted", name, drive.letter()),
    }
}

#[cfg(feature = "storage")]
fn cmd_mv(arg: &str) {
    let Some((from, to)) = arg.split_once(' ') else {
        kprintln!("usage: mv <from> <to>");
        return;
    };
    let (to, from_str) = (to.trim(), from);
    let (df, rf) = crate::fs::resolve_path(from_str);
    let (dt, rt) = crate::fs::resolve_path(to);
    if df != dt {
        kprintln!("mv: can't move between drives ({}: to {}:)", df.letter(), dt.letter());
        return;
    }
    fs_op("mv", from_str, |vol, _| vol.rename(rf, rt));
}

#[cfg(feature = "elf-loader")]
fn cmd_kill(arg: &str) {
    let Ok(pid) = arg.trim().parse::<u64>() else {
        kprintln!("usage: kill <pid>");
        return;
    };
    match crate::sched::kill_process(pid, 128 + 9) {
        Ok(()) => kprintln!("killed pid {}", pid),
        Err(()) => kprintln!("kill: no such running process: {}", pid),
    }
}

#[cfg(feature = "usermode-demo")]
fn cmd_date() {
    let t = crate::drivers::rtc::unix_time();
    let (days, rem) = (t / 86400, t % 86400);
    // civil-from-days (Hinnant)
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    kprintln!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60);
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
fn cmd_run(line: &str) {
    let mut words = line.split_whitespace();
    let Some(path) = words.next() else {
        kprintln!("usage: run <path> [args...]");
        return;
    };
    let args: alloc::vec::Vec<&str> = words.collect();
    match crate::elf::spawn_args("run", path, &args) {
        Ok(pid) => kprintln!("run: {} -> pid {} (runs in the background)", path, pid),
        Err(e) => kprintln!("run: failed: {:?}", e),
    }
}
#[cfg(not(feature = "elf-loader"))]
fn cmd_run(_path: &str) {
    kprintln!("run: `elf-loader` feature not enabled");
}

#[cfg(feature = "net")]
fn cmd_ifconfig() {
    let (Some(c), Some(st)) = (crate::net::config(), crate::net::stats()) else {
        kprintln!("ifconfig: no network interface");
        return;
    };
    kprintln!("eth0: {}  mac {}", c.ip, c.mac);
    kprintln!("      netmask {}  gateway {}", c.netmask, c.gateway.map_or("none".into(), |g| alloc::format!("{}", g)));
    kprintln!("      rx {} frames ({} dropped)  tx {} frames", st.rx_frames, st.rx_dropped, st.tx_frames);
    kprintln!(
        "      arp rx {} / requests {} / failures {}   ip rx {} ({} dropped)",
        st.arp_rx, st.arp_requests_tx, st.arp_failures, st.ip_rx, st.ip_dropped
    );
    kprintln!(
        "      icmp echo requests {} / replies {}   udp rx {} ({} to closed ports)",
        st.icmp_echo_requests_rx, st.icmp_echo_replies_rx, st.udp_rx, st.udp_no_port
    );
}

#[cfg(feature = "net")]
fn cmd_arp() {
    let Some(entries) = crate::net::arp_table() else {
        kprintln!("arp: no network interface");
        return;
    };
    if entries.is_empty() {
        kprintln!("arp: cache is empty");
    }
    let now = crate::net::now_ms();
    for e in entries {
        kprintln!("  {:<15}  {}  (expires in {} s)", e.ip, e.mac, e.expires_ms.saturating_sub(now) / 1000);
    }
}

#[cfg(feature = "net")]
fn cmd_ping(arg: &str) {
    use crate::net::PingError;
    static NEXT_SEQ: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(1);
    const ID: u16 = 0x7368; // "sh"

    let mut args = arg.split_whitespace();
    let Some(Ok(dst)) = args.next().map(str::parse::<core::net::Ipv4Addr>) else {
        kprintln!("usage: ping <a.b.c.d> [count]");
        return;
    };
    let count = args.next().and_then(|n| n.parse::<u16>().ok()).unwrap_or(4).clamp(1, 100);

    let (mut ok, mut sent) = (0u32, 0u32);
    for _ in 0..count {
        let seq = NEXT_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        sent += 1;
        match crate::net::ping(dst, ID, seq, 2_000) {
            Ok((r, rtt)) => {
                ok += 1;
                kprintln!("{} bytes from {}: seq={} ttl={} time={} ms", r.len, r.from, seq, r.ttl, rtt);
            }
            Err(PingError::Timeout) => kprintln!("request timed out: seq={}", seq),
            Err(e) => {
                kprintln!("ping: {:?}", e);
                return;
            }
        }
        // ~1 s between requests, like ping(8) — but don't wait after the last.
        if sent < count as u32 {
            let until = crate::net::now_ms() + 1_000;
            while crate::net::now_ms() < until {
                crate::sched::yield_now();
            }
        }
    }
    kprintln!("{} sent, {} received", sent, ok);
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
        #[cfg(feature = "storage")]
        "mkdir" => fs_op("mkdir", arg, |v, p| v.create_dir(p)),
        #[cfg(feature = "storage")]
        "rmdir" => fs_op("rmdir", arg, |v, p| v.remove_dir(p)),
        #[cfg(feature = "storage")]
        "rm" => fs_op("rm", arg, |v, p| v.delete_file(p)),
        #[cfg(feature = "storage")]
        "mv" => cmd_mv(arg),
        #[cfg(feature = "elf-loader")]
        "kill" => cmd_kill(arg),
        #[cfg(feature = "usermode-demo")]
        "date" => cmd_date(),
        #[cfg(feature = "net")]
        "ifconfig" => cmd_ifconfig(),
        #[cfg(feature = "net")]
        "arp" => cmd_arp(),
        #[cfg(feature = "net")]
        "ping" => cmd_ping(arg),
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
