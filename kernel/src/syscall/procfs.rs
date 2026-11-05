//! `/proc`: read-only text files describing the running kernel. This is how
//! userspace tools (`ps`, `meminfo`, `drives`, `ifconfig`, ...) get at state
//! that only the kernel has. A file's text is generated when it's opened
//! and stays that snapshot for as long as it's open. Directories aren't
//! listable, so the set of names is just what [`read`] matches.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// The text of `/proc/<name>`, or `None` if there's no such file.
pub fn read(name: &str) -> Option<Vec<u8>> {
    let mut s = String::new();
    match name {
        "dmesg" => dmesg(&mut s),
        "tasks" => tasks(&mut s),
        "meminfo" => meminfo(&mut s),
        "uptime" => uptime(&mut s),
        "drives" => drives(&mut s),
        #[cfg(feature = "net")]
        "ifconfig" => ifconfig(&mut s),
        #[cfg(feature = "net")]
        "arp" => arp(&mut s),
        _ => return None,
    }
    Some(s.into_bytes())
}

/// The kernel log. Once the buffer has wrapped, its oldest line has lost
/// its start (which may even be in the middle of a character), so that
/// line is dropped.
fn dmesg(s: &mut String) {
    let mut bytes = Vec::with_capacity(crate::log::LOG_BUFFER_SIZE);
    crate::log::read_log(|b| bytes.push(b));
    let start = match crate::log::log_wrapped() {
        true => bytes.iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| i + 1),
        false => 0,
    };
    s.push_str(&String::from_utf8_lossy(&bytes[start..]));
}

fn tasks(s: &mut String) {
    let _ = writeln!(s, "  PID  PPID  STATE     NAME");
    #[cfg(feature = "elf-loader")]
    crate::sched::for_each_process(|pid, ppid, name, state| {
        let state = alloc::format!("{:?}", state);
        if pid == 0 {
            let _ = writeln!(s, "    -     -  {:<8}  [{}]", state, name);
        } else {
            let _ = writeln!(s, "{:>5} {:>5}  {:<8}  {}", pid, ppid, state, name);
        }
    });
}

fn meminfo(s: &mut String) {
    #[cfg(feature = "frame-alloc")]
    {
        let free = crate::mem::frame::free_count();
        let total = crate::mem::frame::total_frames();
        let _ = writeln!(s, "frames: {}/{} free ({} KiB / {} KiB)", free, total, free * 4, total * 4);
    }
    #[cfg(not(feature = "frame-alloc"))]
    let _ = s;
}

fn uptime(s: &mut String) {
    use crate::arch::interrupts::{TIMER_HZ, TIMER_TICKS};
    let ticks = TIMER_TICKS.load(core::sync::atomic::Ordering::Relaxed);
    let hz = TIMER_HZ as u64;
    let _ = writeln!(s, "uptime: {}.{:02}s ({} ticks @ {} Hz)", ticks / hz, (ticks % hz) * 100 / hz, ticks, hz);
}

fn drives(s: &mut String) {
    let _ = writeln!(s, "DRIVE  TYPE   SIZE      FREE      MODE  SOURCE / LABEL");
    let mib = |b: u64| alloc::format!("{} MiB", b >> 20);
    for d in crate::fs::drives() {
        let free = if d.free == 0 && d.total == 0 { String::from("?") } else { mib(d.free) };
        let _ = writeln!(
            s,
            "{}:     {:<6} {:<9} {:<9} {:<5} {}{}{}",
            d.drive.letter(),
            d.fs_type,
            mib(d.total),
            free,
            if d.read_only { "ro" } else { "rw" },
            d.source,
            if d.label.is_empty() { "" } else { "  " },
            d.label
        );
    }
}

#[cfg(feature = "net")]
fn ifconfig(s: &mut String) {
    let (Some(c), Some(st)) = (crate::net::config(), crate::net::stats()) else {
        let _ = writeln!(s, "no network interface");
        return;
    };
    let gateway = c.gateway.map_or(String::from("none"), |g| alloc::format!("{}", g));
    let _ = writeln!(s, "eth0: {}  mac {}", c.ip, c.mac);
    let _ = writeln!(s, "      netmask {}  gateway {}", c.netmask, gateway);
    let _ = writeln!(s, "      rx {} frames ({} dropped)  tx {} frames", st.rx_frames, st.rx_dropped, st.tx_frames);
    let _ = writeln!(
        s,
        "      arp rx {} / requests {} / failures {}   ip rx {} ({} dropped)",
        st.arp_rx, st.arp_requests_tx, st.arp_failures, st.ip_rx, st.ip_dropped
    );
    let _ = writeln!(
        s,
        "      icmp echo requests {} / replies {}   udp rx {} ({} to closed ports)",
        st.icmp_echo_requests_rx, st.icmp_echo_replies_rx, st.udp_rx, st.udp_no_port
    );
}

#[cfg(feature = "net")]
fn arp(s: &mut String) {
    let Some(entries) = crate::net::arp_table() else {
        let _ = writeln!(s, "no network interface");
        return;
    };
    let now = crate::net::now_ms();
    for e in entries {
        let _ = writeln!(s, "{:<15}  {}  (expires in {} s)", e.ip, e.mac, e.expires_ms.saturating_sub(now) / 1000);
    }
}
