//! Kernel glue for the network stack.
//!
//! The protocol logic lives in the `basicos-net` crate (see its docs); a
//! NIC driver implements its `NetDevice`; this module owns the one global
//! `Stack` + device pair, runs the polling task that pumps frames between
//! them, and exposes the few entry points the rest of the kernel (the
//! shell today, socket syscalls eventually) needs.
//!
//! Until DHCP exists, addressing is static and set for QEMU's user-mode
//! networking (`-netdev user`): the guest is 10.0.2.15 on a /24 whose
//! gateway/host-side is 10.0.2.2.

use crate::arch::interrupts::{TIMER_HZ, TIMER_TICKS};
use crate::drivers::{pci, rtl8139::Rtl8139};
use crate::sync::Spinlock;
use crate::{pr_info, pr_warn, sched};
use alloc::vec::Vec;
use basicos_net::stack::{ArpEntry, EchoReply, SendError, Stats};
use basicos_net::{Config, NetDevice, Stack};
use core::net::Ipv4Addr;
use core::sync::atomic::Ordering;

pub const DEFAULT_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
pub const DEFAULT_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
pub const DEFAULT_GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

struct Net {
    dev: Rtl8139,
    stack: Stack,
}

static NET: Spinlock<Option<Net>> = Spinlock::new(None);

/// Milliseconds since boot, at PIT-tick resolution.
pub fn now_ms() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed) * 1000 / TIMER_HZ as u64
}

/// Find a NIC and, if there is one, bring the stack up and spawn the
/// `net` task. A machine without a supported NIC just logs that and
/// carries on — networking is optional.
pub fn init() {
    let mut count = 0;
    pci::enumerate(|d| {
        count += 1;
        pr_info!(
            "pci: {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}",
            d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass
        );
    });
    pr_info!("pci: {} device(s)", count);

    let Some(dev) = Rtl8139::probe() else {
        pr_info!("net: no supported NIC found (looked for an RTL8139)");
        return;
    };
    let mac = dev.mac();
    pr_info!("net: rtl8139 at io {:#x}, mac {}", dev.io_base(), mac);

    let stack = Stack::new(Config {
        mac,
        ip: DEFAULT_IP,
        netmask: DEFAULT_NETMASK,
        gateway: Some(DEFAULT_GATEWAY),
    });
    *NET.lock() = Some(Net { dev, stack });

    if sched::spawn("net", net_task).is_none() {
        pr_warn!("net: scheduler task table is full; the stack won't run");
    }
}

/// Run `f` against the stack, then pump: anything `f` queued goes out to
/// the device immediately, and anything the device has received is
/// handled. `None` if there's no NIC.
pub fn with_stack<R>(f: impl FnOnce(&mut Stack) -> R) -> Option<R> {
    let mut guard = NET.lock();
    let net = guard.as_mut()?;
    let r = f(&mut net.stack);
    net.stack.pump(now_ms(), &mut net.dev);
    Some(r)
}

pub fn is_up() -> bool {
    NET.lock().is_some()
}

pub fn config() -> Option<Config> {
    with_stack(|s| *s.config())
}

pub fn stats() -> Option<Stats> {
    with_stack(|s| s.stats())
}

pub fn arp_table() -> Option<Vec<ArpEntry>> {
    with_stack(|s| s.arp_table().to_vec())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingError {
    NoDevice,
    Send(SendError),
    Timeout,
}

/// Send one echo request and wait (yielding, so the `net` task keeps
/// pumping) up to `timeout_ms` for its reply. Returns the reply and the
/// round-trip time — at tick resolution, so anything under one PIT tick
/// (10 ms) reads as 0.
pub fn ping(dst: Ipv4Addr, id: u16, seq: u16, timeout_ms: u64) -> Result<(EchoReply, u64), PingError> {
    let sent = now_ms();
    with_stack(|s| s.ping(sent, dst, id, seq, 32))
        .ok_or(PingError::NoDevice)?
        .map_err(PingError::Send)?;
    while now_ms().saturating_sub(sent) < timeout_ms {
        if let Some(Some(reply)) = with_stack(|s| s.take_echo_reply(id, seq)) {
            return Ok((reply, reply.at_ms.saturating_sub(sent)));
        }
        sched::yield_now();
    }
    Err(PingError::Timeout)
}

/// UDP echo service (RFC 862): sends every datagram straight back to its
/// sender. Mostly here so UDP has something on the other end to talk to
/// — it's also what the end-to-end test (`make net-test`) exercises.
pub const ECHO_PORT: u16 = 7;

fn serve_udp_echo(s: &mut Stack) {
    while let Some(d) = s.recv_udp(ECHO_PORT) {
        let _ = s.send_udp(now_ms(), d.src, ECHO_PORT, d.src_port, &d.data);
    }
}

/// Polls the NIC and drives the stack's timers, forever.
extern "C" fn net_task() -> ! {
    const SELFTEST_ID: u16 = 0x6e74;
    let _ = with_stack(|s| s.bind_udp(ECHO_PORT));

    // Boot-time smoke test: resolve and ping the gateway, and say what
    // happened. Same task as the pump, so it can't use `ping()` (which
    // waits on the pump); it checks for the reply on each pass instead.
    let sent = now_ms();
    let _ = with_stack(|s| s.ping(sent, DEFAULT_GATEWAY, SELFTEST_ID, 1, 32));
    let mut reported = false;

    loop {
        // `with_stack` pumps after the closure: received frames are
        // handled and anything queued is transmitted.
        let reply = with_stack(|s| {
            serve_udp_echo(s);
            s.take_echo_reply(SELFTEST_ID, 1)
        })
        .flatten();

        if !reported {
            match reply {
                Some(r) => {
                    pr_info!(
                        "net: gateway {} answers ping ({} ms, ttl {})",
                        r.from,
                        r.at_ms.saturating_sub(sent),
                        r.ttl
                    );
                    reported = true;
                }
                None if now_ms().saturating_sub(sent) > 5_000 => {
                    pr_warn!("net: no reply from gateway {} after 5 s", DEFAULT_GATEWAY);
                    reported = true;
                }
                None => {}
            }
        }
        sched::yield_now();
    }
}
