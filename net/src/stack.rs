//! The IPv4 stack: a sans-IO state machine.
//!
//! [`Stack`] never touches a device or a clock. Incoming frames go in
//! through [`Stack::handle_frame`]; outgoing frames accumulate in an
//! internal queue and come out through [`Stack::pop_tx`]; time is passed
//! in (as milliseconds) wherever it matters. That makes the whole thing
//! testable on a host with two stacks wired to each other — see the tests
//! at the bottom — and leaves the kernel to supply just a [`NetDevice`]
//! and a tick source. [`Stack::pump`] is the one-call version of that
//! glue.
//!
//! Scope: Ethernet, ARP (cache, retries, pending-packet queue), IPv4
//! (no fragmentation, no forwarding), ICMP echo + port-unreachable, UDP,
//! and loopback (127.0.0.0/8 and the stack's own address never reach the
//! device). TCP, DHCP and DNS are layers above this.

use crate::ethernet::{self, MacAddr};
use crate::{arp, icmp, ipv4, udp};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

pub const MTU: usize = 1500;

const ARP_TTL_MS: u64 = 60_000;
const ARP_RETRY_MS: u64 = 1_000;
const ARP_MAX_TRIES: u32 = 3;
const ARP_CACHE_MAX: usize = 32;
/// Packets held while waiting for ARP to resolve their next hop.
const MAX_PENDING: usize = 16;
/// Received-but-unread datagrams/echo replies kept per queue.
const MAX_QUEUE: usize = 32;
/// Bound on loopback re-entry (a reply generating a reply generating...).
const MAX_LOOPBACK_STEPS: usize = 64;
/// Raw sockets open at once, stack-wide (each costs a copy of every
/// matching packet).
pub const MAX_RAW_SOCKETS: usize = 32;

/// What a NIC driver provides. Frames are raw Ethernet, without the FCS;
/// padding short frames up to the 60-byte minimum is the driver's job.
pub trait NetDevice {
    fn mac(&self) -> MacAddr;
    fn send(&mut self, frame: &[u8]) -> Result<(), DeviceError>;
    /// Copy the next received frame into `buf` and return its length, or
    /// `None` if nothing is waiting.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    /// Frame longer than the device can transmit.
    TooLong,
    /// No transmit slot free (or the hardware timed out).
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub mac: MacAddr,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Option<Ipv4Addr>,
}

impl Config {
    fn same_subnet(&self, other: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        u32::from(self.ip) & mask == u32::from(other) & mask
    }

    fn subnet_broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.ip) | !u32::from(self.netmask))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// Off-subnet destination and no gateway configured.
    NoRoute,
    /// Payload doesn't fit in one MTU; fragmentation isn't supported.
    TooBig,
    /// Too many packets already waiting on ARP.
    QueueFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    InUse,
    /// Port 0 isn't bindable.
    InvalidPort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EchoReply {
    pub from: Ipv4Addr,
    pub id: u16,
    pub seq: u16,
    pub ttl: u8,
    /// Echo payload length in bytes.
    pub len: usize,
    /// The `now_ms` the reply was handled at.
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpDatagram {
    pub src: Ipv4Addr,
    pub src_port: u16,
    pub dst: Ipv4Addr,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArpEntry {
    pub ip: Ipv4Addr,
    pub mac: MacAddr,
    pub expires_ms: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub rx_frames: u64,
    pub tx_frames: u64,
    /// Frames dropped before reaching a protocol (wrong MAC, unknown
    /// EtherType, runt).
    pub rx_dropped: u64,
    pub arp_rx: u64,
    pub arp_requests_tx: u64,
    /// Next hops that never answered ARP (their queued packets dropped).
    pub arp_failures: u64,
    pub ip_rx: u64,
    /// Bad checksum/length, fragments, wrong destination, unsupported
    /// protocol.
    pub ip_dropped: u64,
    pub icmp_echo_requests_rx: u64,
    pub icmp_echo_replies_rx: u64,
    pub udp_rx: u64,
    pub udp_no_port: u64,
}

struct Pending {
    next_hop: Ipv4Addr,
    packet: Vec<u8>,
}

struct ArpQuery {
    ip: Ipv4Addr,
    last_sent_ms: u64,
    tries: u32,
}

struct UdpSocket {
    port: u16,
    queue: VecDeque<UdpDatagram>,
}

/// A packet delivered to a raw socket: the whole IPv4 packet, header
/// included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPacket {
    pub src: Ipv4Addr,
    pub packet: Vec<u8>,
}

struct RawSocket {
    id: u32,
    protocol: u8,
    queue: VecDeque<RawPacket>,
}

pub struct Stack {
    cfg: Config,
    arp: Vec<ArpEntry>,
    queries: Vec<ArpQuery>,
    pending: Vec<Pending>,
    tx: VecDeque<Vec<u8>>,
    loopback: VecDeque<Vec<u8>>,
    echo_replies: VecDeque<EchoReply>,
    sockets: Vec<UdpSocket>,
    raw_sockets: Vec<RawSocket>,
    next_raw_id: u32,
    ip_id: u16,
    stats: Stats,
}

impl Stack {
    pub fn new(cfg: Config) -> Stack {
        Stack {
            cfg,
            arp: Vec::new(),
            queries: Vec::new(),
            pending: Vec::new(),
            tx: VecDeque::new(),
            loopback: VecDeque::new(),
            echo_replies: VecDeque::new(),
            sockets: Vec::new(),
            raw_sockets: Vec::new(),
            next_raw_id: 1,
            ip_id: 1,
            stats: Stats::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Change address/netmask/gateway (what a DHCP lease will do). The ARP
    /// cache and anything waiting on it are dropped: they were resolved
    /// against the old subnet.
    pub fn set_addr(&mut self, ip: Ipv4Addr, netmask: Ipv4Addr, gateway: Option<Ipv4Addr>) {
        self.cfg.ip = ip;
        self.cfg.netmask = netmask;
        self.cfg.gateway = gateway;
        self.arp.clear();
        self.queries.clear();
        self.pending.clear();
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn arp_table(&self) -> &[ArpEntry] {
        &self.arp
    }

    /// Next frame the device should transmit, if any.
    pub fn pop_tx(&mut self) -> Option<Vec<u8>> {
        let f = self.tx.pop_front()?;
        self.stats.tx_frames += 1;
        Some(f)
    }

    // --- receive path -------------------------------------------------------

    pub fn handle_frame(&mut self, now_ms: u64, frame: &[u8]) {
        self.stats.rx_frames += 1;
        let Some(f) = ethernet::parse(frame) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if f.dst != self.cfg.mac && !f.dst.is_broadcast() {
            self.stats.rx_dropped += 1;
            return;
        }
        match f.ethertype {
            ethernet::ETHERTYPE_ARP => self.handle_arp(now_ms, f.payload),
            ethernet::ETHERTYPE_IPV4 => self.handle_ip(now_ms, f.payload),
            _ => self.stats.rx_dropped += 1,
        }
        self.settle(now_ms);
    }

    fn handle_arp(&mut self, now: u64, payload: &[u8]) {
        let Some(p) = arp::parse(payload) else {
            self.stats.rx_dropped += 1;
            return;
        };
        self.stats.arp_rx += 1;

        // Learn the sender if it's talking to us, or we already track it —
        // but don't let arbitrary broadcast chatter fill the cache.
        let targeted = p.target_ip == self.cfg.ip;
        let tracked = self.arp.iter().any(|e| e.ip == p.sender_ip)
            || self.queries.iter().any(|q| q.ip == p.sender_ip);
        if (targeted || tracked) && !p.sender_ip.is_unspecified() {
            self.learn(now, p.sender_ip, p.sender_mac);
            self.complete_query(now, p.sender_ip);
        }

        if p.op == arp::OP_REQUEST && targeted {
            let reply = arp::Packet {
                op: arp::OP_REPLY,
                sender_mac: self.cfg.mac,
                sender_ip: self.cfg.ip,
                target_mac: p.sender_mac,
                target_ip: p.sender_ip,
            };
            self.queue_frame(p.sender_mac, ethernet::ETHERTYPE_ARP, &arp::build(&reply));
        }
    }

    fn handle_ip(&mut self, now: u64, pkt: &[u8]) {
        self.stats.ip_rx += 1;
        let (h, payload) = match ipv4::parse(pkt) {
            Ok(x) => x,
            Err(_) => {
                self.stats.ip_dropped += 1;
                return;
            }
        };
        if h.is_fragment {
            self.stats.ip_dropped += 1;
            return;
        }
        let for_us = h.dst == self.cfg.ip || h.dst.is_loopback();
        let broadcast = h.dst == Ipv4Addr::BROADCAST || h.dst == self.cfg.subnet_broadcast();
        if !for_us && !broadcast {
            // Not a router: nothing to forward.
            self.stats.ip_dropped += 1;
            return;
        }
        // Raw sockets get a copy; the stack still handles the packet as
        // usual, so a program watching ICMP doesn't stop us answering pings.
        let delivered = self.deliver_raw(h.src, h.protocol, &pkt[..h.total_len]);
        match h.protocol {
            ipv4::PROTO_ICMP => self.handle_icmp(now, &h, payload, for_us),
            ipv4::PROTO_UDP => self.handle_udp(now, &h, payload, pkt, for_us),
            _ if delivered => {}
            _ => self.stats.ip_dropped += 1,
        }
    }

    /// Queue `pkt` on every raw socket for `protocol`, dropping it for any
    /// socket whose queue is full. True if at least one socket is open.
    fn deliver_raw(&mut self, src: Ipv4Addr, protocol: u8, pkt: &[u8]) -> bool {
        let mut any = false;
        for sock in self.raw_sockets.iter_mut().filter(|s| s.protocol == protocol) {
            any = true;
            if sock.queue.len() < MAX_QUEUE {
                sock.queue.push_back(RawPacket { src, packet: pkt.to_vec() });
            }
        }
        any
    }

    fn handle_icmp(&mut self, now: u64, h: &ipv4::Header, payload: &[u8], for_us: bool) {
        let Some(m) = icmp::parse(payload) else {
            self.stats.ip_dropped += 1;
            return;
        };
        match m.ty {
            // Echo to a broadcast address is ignored (as Linux does by
            // default): answering would let anyone use us to amplify.
            icmp::TYPE_ECHO_REQUEST if for_us => {
                self.stats.icmp_echo_requests_rx += 1;
                let reply = icmp::build_echo(icmp::TYPE_ECHO_REPLY, m.echo_id(), m.echo_seq(), m.data);
                let _ = self.send_ip(now, h.src, ipv4::PROTO_ICMP, &reply);
            }
            icmp::TYPE_ECHO_REPLY => {
                self.stats.icmp_echo_replies_rx += 1;
                if self.echo_replies.len() >= MAX_QUEUE {
                    self.echo_replies.pop_front();
                }
                self.echo_replies.push_back(EchoReply {
                    from: h.src,
                    id: m.echo_id(),
                    seq: m.echo_seq(),
                    ttl: h.ttl,
                    len: m.data.len(),
                    at_ms: now,
                });
            }
            _ => {}
        }
    }

    fn handle_udp(&mut self, now: u64, h: &ipv4::Header, payload: &[u8], pkt: &[u8], for_us: bool) {
        let d = match udp::parse(h.src, h.dst, payload) {
            Ok(d) => d,
            Err(_) => {
                self.stats.ip_dropped += 1;
                return;
            }
        };
        self.stats.udp_rx += 1;
        if let Some(sock) = self.sockets.iter_mut().find(|s| s.port == d.dst_port) {
            if sock.queue.len() < MAX_QUEUE {
                sock.queue.push_back(UdpDatagram {
                    src: h.src,
                    src_port: d.src_port,
                    dst: h.dst,
                    data: d.data.to_vec(),
                });
            }
            return;
        }
        self.stats.udp_no_port += 1;
        if for_us {
            // ICMP port unreachable, carrying the offending IP header
            // plus the first 8 bytes of its payload (RFC 792), so the
            // sender can tell which of its sockets it concerned.
            let quote = &pkt[..(h.header_len + 8).min(h.total_len)];
            let msg = icmp::build(
                icmp::TYPE_DEST_UNREACHABLE,
                icmp::CODE_PORT_UNREACHABLE,
                [0; 4],
                quote,
            );
            let _ = self.send_ip(now, h.src, ipv4::PROTO_ICMP, &msg);
        }
    }

    // --- transmit path ------------------------------------------------------

    fn queue_frame(&mut self, dst: MacAddr, ethertype: u16, payload: &[u8]) {
        let frame = ethernet::build(dst, self.cfg.mac, ethertype, payload);
        self.tx.push_back(frame);
    }

    fn send_arp_request(&mut self, target: Ipv4Addr) {
        let req = arp::Packet {
            op: arp::OP_REQUEST,
            sender_mac: self.cfg.mac,
            sender_ip: self.cfg.ip,
            target_mac: MacAddr::ZERO,
            target_ip: target,
        };
        self.stats.arp_requests_tx += 1;
        self.queue_frame(MacAddr::BROADCAST, ethernet::ETHERTYPE_ARP, &arp::build(&req));
    }

    fn source_for(&self, dst: Ipv4Addr) -> Ipv4Addr {
        if dst.is_loopback() {
            Ipv4Addr::LOCALHOST
        } else {
            self.cfg.ip
        }
    }

    fn send_ip(&mut self, now: u64, dst: Ipv4Addr, protocol: u8, payload: &[u8]) -> Result<(), SendError> {
        if ipv4::HEADER_LEN + payload.len() > MTU {
            return Err(SendError::TooBig);
        }
        let src = self.source_for(dst);
        let id = self.ip_id;
        self.ip_id = self.ip_id.wrapping_add(1);
        let pkt = ipv4::build(src, dst, protocol, 64, id, payload);

        // Traffic to ourselves never reaches the wire.
        if dst.is_loopback() || dst == self.cfg.ip {
            self.loopback.push_back(pkt);
            return Ok(());
        }
        if dst == Ipv4Addr::BROADCAST || dst == self.cfg.subnet_broadcast() {
            self.queue_frame(MacAddr::BROADCAST, ethernet::ETHERTYPE_IPV4, &pkt);
            return Ok(());
        }

        let hop = if self.cfg.same_subnet(dst) {
            dst
        } else {
            self.cfg.gateway.ok_or(SendError::NoRoute)?
        };
        match self.arp_lookup(now, hop) {
            Some(mac) => self.queue_frame(mac, ethernet::ETHERTYPE_IPV4, &pkt),
            None => {
                if self.pending.len() >= MAX_PENDING {
                    return Err(SendError::QueueFull);
                }
                self.pending.push(Pending { next_hop: hop, packet: pkt });
                if !self.queries.iter().any(|q| q.ip == hop) {
                    self.queries.push(ArpQuery { ip: hop, last_sent_ms: now, tries: 1 });
                    self.send_arp_request(hop);
                }
            }
        }
        Ok(())
    }

    // --- ARP cache ----------------------------------------------------------

    fn arp_lookup(&self, now: u64, ip: Ipv4Addr) -> Option<MacAddr> {
        self.arp
            .iter()
            .find(|e| e.ip == ip && e.expires_ms > now)
            .map(|e| e.mac)
    }

    fn learn(&mut self, now: u64, ip: Ipv4Addr, mac: MacAddr) {
        if mac.is_multicast() || mac == MacAddr::ZERO {
            return;
        }
        let expires_ms = now + ARP_TTL_MS;
        if let Some(e) = self.arp.iter_mut().find(|e| e.ip == ip) {
            e.mac = mac;
            e.expires_ms = expires_ms;
            return;
        }
        if self.arp.len() >= ARP_CACHE_MAX {
            // Evict whichever entry is closest to expiring.
            if let Some(i) = (0..self.arp.len()).min_by_key(|&i| self.arp[i].expires_ms) {
                self.arp.swap_remove(i);
            }
        }
        self.arp.push(ArpEntry { ip, mac, expires_ms });
    }

    /// `ip` just resolved: send everything that was waiting on it.
    fn complete_query(&mut self, now: u64, ip: Ipv4Addr) {
        let Some(mac) = self.arp_lookup(now, ip) else { return };
        self.queries.retain(|q| q.ip != ip);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].next_hop == ip {
                let p = self.pending.remove(i);
                self.queue_frame(mac, ethernet::ETHERTYPE_IPV4, &p.packet);
            } else {
                i += 1;
            }
        }
    }

    // --- timers and the public send API -------------------------------------

    /// Drive time-based work: expire ARP entries, retry unanswered ARP
    /// requests, and give up (dropping the packets queued behind them) on
    /// next hops that never answer.
    pub fn poll(&mut self, now_ms: u64) {
        self.arp.retain(|e| e.expires_ms > now_ms);

        let mut resend = Vec::new();
        let mut failed = Vec::new();
        for q in &mut self.queries {
            if now_ms.saturating_sub(q.last_sent_ms) >= ARP_RETRY_MS {
                if q.tries >= ARP_MAX_TRIES {
                    failed.push(q.ip);
                } else {
                    q.tries += 1;
                    q.last_sent_ms = now_ms;
                    resend.push(q.ip);
                }
            }
        }
        for ip in resend {
            self.send_arp_request(ip);
        }
        for ip in failed {
            self.stats.arp_failures += 1;
            self.queries.retain(|q| q.ip != ip);
            self.pending.retain(|p| p.next_hop != ip);
        }
        self.settle(now_ms);
    }

    /// Send an ICMP echo request with a `payload_len`-byte pattern. The
    /// reply, if one comes, shows up via [`Stack::take_echo_reply`].
    pub fn ping(&mut self, now_ms: u64, dst: Ipv4Addr, id: u16, seq: u16, payload_len: usize) -> Result<(), SendError> {
        let data: Vec<u8> = (0..payload_len).map(|i| i as u8).collect();
        let msg = icmp::build_echo(icmp::TYPE_ECHO_REQUEST, id, seq, &data);
        self.send_ip(now_ms, dst, ipv4::PROTO_ICMP, &msg)?;
        self.settle(now_ms);
        Ok(())
    }

    pub fn take_echo_reply(&mut self, id: u16, seq: u16) -> Option<EchoReply> {
        let i = self.echo_replies.iter().position(|r| r.id == id && r.seq == seq)?;
        self.echo_replies.remove(i)
    }

    pub fn bind_udp(&mut self, port: u16) -> Result<(), BindError> {
        if port == 0 {
            return Err(BindError::InvalidPort);
        }
        if self.sockets.iter().any(|s| s.port == port) {
            return Err(BindError::InUse);
        }
        self.sockets.push(UdpSocket { port, queue: VecDeque::new() });
        Ok(())
    }

    pub fn unbind_udp(&mut self, port: u16) {
        self.sockets.retain(|s| s.port != port);
    }

    /// Payload length of the next queued datagram on `port`, if any —
    /// `poll`/`FIONREAD` without consuming it.
    pub fn udp_next_len(&self, port: u16) -> Option<usize> {
        self.sockets.iter().find(|s| s.port == port)?.queue.front().map(|d| d.data.len())
    }

    /// Whether `port` is currently bound.
    pub fn udp_is_bound(&self, port: u16) -> bool {
        self.sockets.iter().any(|s| s.port == port)
    }

    pub fn recv_udp(&mut self, port: u16) -> Option<UdpDatagram> {
        self.sockets.iter_mut().find(|s| s.port == port)?.queue.pop_front()
    }

    pub fn send_udp(&mut self, now_ms: u64, dst: Ipv4Addr, src_port: u16, dst_port: u16, data: &[u8]) -> Result<(), SendError> {
        let src = self.source_for(dst);
        let dgram = udp::build(src, dst, src_port, dst_port, data);
        self.send_ip(now_ms, dst, ipv4::PROTO_UDP, &dgram)?;
        self.settle(now_ms);
        Ok(())
    }

    /// Open a raw socket for an IP protocol. `None` if too many are open.
    pub fn open_raw(&mut self, protocol: u8) -> Option<u32> {
        if self.raw_sockets.len() >= MAX_RAW_SOCKETS {
            return None;
        }
        let id = self.next_raw_id;
        self.next_raw_id = self.next_raw_id.wrapping_add(1);
        self.raw_sockets.push(RawSocket { id, protocol, queue: VecDeque::new() });
        Some(id)
    }

    pub fn close_raw(&mut self, id: u32) {
        self.raw_sockets.retain(|s| s.id != id);
    }

    /// The size of the next packet waiting on raw socket `id`, if any.
    pub fn raw_next_len(&self, id: u32) -> Option<usize> {
        self.raw_sockets.iter().find(|s| s.id == id)?.queue.front().map(|p| p.packet.len())
    }

    pub fn recv_raw(&mut self, id: u32) -> Option<RawPacket> {
        self.raw_sockets.iter_mut().find(|s| s.id == id)?.queue.pop_front()
    }

    /// Send `data` as the payload of an IP packet. The stack writes the IP
    /// header, so the source address is always our own.
    pub fn send_raw(&mut self, now_ms: u64, dst: Ipv4Addr, protocol: u8, data: &[u8]) -> Result<(), SendError> {
        self.send_ip(now_ms, dst, protocol, data)?;
        self.settle(now_ms);
        Ok(())
    }

    /// Deliver queued loopback packets (which may themselves queue more —
    /// an echo request's reply is also loopback traffic).
    fn settle(&mut self, now: u64) {
        for _ in 0..MAX_LOOPBACK_STEPS {
            let Some(pkt) = self.loopback.pop_front() else { return };
            self.handle_ip(now, &pkt);
        }
        self.loopback.clear();
    }

    /// One-call glue: feed everything the device has received to the
    /// stack, run timers, and transmit everything the stack produced.
    pub fn pump(&mut self, now_ms: u64, dev: &mut dyn NetDevice) {
        let mut buf = [0u8; 2048];
        // Bounded so a flooded NIC can't starve the caller's other work.
        for _ in 0..64 {
            match dev.recv(&mut buf) {
                Some(n) => self.handle_frame(now_ms, &buf[..n]),
                None => break,
            }
        }
        self.poll(now_ms);
        while let Some(f) = self.pop_tx() {
            let _ = dev.send(&f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque as StdDeque;
    use std::vec;

    const NET: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
    const GW: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 254);

    fn cfg(n: u8) -> Config {
        Config {
            mac: MacAddr([2, 0, 0, 0, 0, n]),
            ip: Ipv4Addr::new(10, 0, 0, n),
            netmask: NET,
            gateway: Some(GW),
        }
    }

    fn pair() -> (Stack, Stack) {
        (Stack::new(cfg(1)), Stack::new(cfg(2)))
    }

    /// Shuttle frames between two stacks until neither has anything to send.
    fn exchange(a: &mut Stack, b: &mut Stack, now: u64) {
        for _ in 0..16 {
            let mut moved = false;
            while let Some(f) = a.pop_tx() {
                b.handle_frame(now, &f);
                moved = true;
            }
            while let Some(f) = b.pop_tx() {
                a.handle_frame(now, &f);
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("frames still bouncing after 16 rounds");
    }

    fn arp_of(frame: &[u8]) -> arp::Packet {
        let f = ethernet::parse(frame).unwrap();
        assert_eq!(f.ethertype, ethernet::ETHERTYPE_ARP);
        arp::parse(f.payload).unwrap()
    }

    #[test]
    fn udp_peek_and_bound_queries() {
        let mut s = Stack::new(Config {
            mac: MacAddr([2, 0, 0, 0, 0, 1]),
            ip: Ipv4Addr::new(10, 0, 0, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: None,
        });
        assert!(!s.udp_is_bound(9));
        s.bind_udp(9).unwrap();
        assert!(s.udp_is_bound(9));
        assert_eq!(s.udp_next_len(9), None);
        s.send_udp(0, Ipv4Addr::LOCALHOST, 5, 9, b"abc").unwrap();
        assert_eq!(s.udp_next_len(9), Some(3));
        assert_eq!(s.recv_udp(9).unwrap().data, b"abc");
        assert_eq!(s.udp_next_len(9), None);
        s.unbind_udp(9);
        assert!(!s.udp_is_bound(9));
    }

    #[test]
    fn answers_arp_requests_for_its_address_and_learns_the_asker() {
        let mut s = Stack::new(cfg(1));
        let req = arp::Packet {
            op: arp::OP_REQUEST,
            sender_mac: MacAddr([2, 0, 0, 0, 0, 9]),
            sender_ip: Ipv4Addr::new(10, 0, 0, 9),
            target_mac: MacAddr::ZERO,
            target_ip: Ipv4Addr::new(10, 0, 0, 1),
        };
        let frame = ethernet::build(MacAddr::BROADCAST, req.sender_mac, ethernet::ETHERTYPE_ARP, &arp::build(&req));
        s.handle_frame(0, &frame);

        let out = s.pop_tx().expect("a reply");
        let eth = ethernet::parse(&out).unwrap();
        assert_eq!(eth.dst, req.sender_mac); // unicast back to the asker
        let reply = arp_of(&out);
        assert_eq!(reply.op, arp::OP_REPLY);
        assert_eq!((reply.sender_mac, reply.sender_ip), (cfg(1).mac, cfg(1).ip));
        assert_eq!((reply.target_mac, reply.target_ip), (req.sender_mac, req.sender_ip));
        assert!(s.pop_tx().is_none());
        assert_eq!(s.arp_table().len(), 1); // learned from the request
    }

    #[test]
    fn ignores_arp_for_other_addresses_and_does_not_cache_chatter() {
        let mut s = Stack::new(cfg(1));
        let req = arp::Packet {
            op: arp::OP_REQUEST,
            sender_mac: MacAddr([2, 0, 0, 0, 0, 9]),
            sender_ip: Ipv4Addr::new(10, 0, 0, 9),
            target_mac: MacAddr::ZERO,
            target_ip: Ipv4Addr::new(10, 0, 0, 77), // someone else
        };
        let frame = ethernet::build(MacAddr::BROADCAST, req.sender_mac, ethernet::ETHERTYPE_ARP, &arp::build(&req));
        s.handle_frame(0, &frame);
        assert!(s.pop_tx().is_none());
        assert!(s.arp_table().is_empty());
    }

    #[test]
    fn ping_resolves_via_arp_then_completes() {
        let (mut a, mut b) = pair();
        a.ping(0, b.config().ip, 0x42, 1, 32).unwrap();

        // Nothing is on the wire but the ARP request; the echo waits.
        let req = a.pop_tx().unwrap();
        assert_eq!(ethernet::parse(&req).unwrap().dst, MacAddr::BROADCAST);
        assert_eq!(arp_of(&req).target_ip, b.config().ip);
        assert!(a.pop_tx().is_none());
        b.handle_frame(0, &req);
        // Deliver B's ARP reply; A then flushes the queued echo request.
        exchange(&mut a, &mut b, 5);

        let r = a.take_echo_reply(0x42, 1).expect("echo reply");
        assert_eq!((r.from, r.len, r.ttl, r.at_ms), (b.config().ip, 32, 64, 5));
        assert!(a.take_echo_reply(0x42, 1).is_none()); // consumed
        assert_eq!(b.stats().icmp_echo_requests_rx, 1);
        assert_eq!(a.stats().arp_requests_tx, 1);
    }

    #[test]
    fn second_ping_uses_the_cache() {
        let (mut a, mut b) = pair();
        a.ping(0, b.config().ip, 1, 1, 8).unwrap();
        exchange(&mut a, &mut b, 0);
        a.ping(10, b.config().ip, 1, 2, 8).unwrap();
        // Already resolved: the very first frame out is the echo itself.
        let f = a.pop_tx().unwrap();
        assert_eq!(ethernet::parse(&f).unwrap().ethertype, ethernet::ETHERTYPE_IPV4);
        b.handle_frame(10, &f);
        exchange(&mut a, &mut b, 10);
        assert!(a.take_echo_reply(1, 2).is_some());
        assert_eq!(a.stats().arp_requests_tx, 1);
    }

    #[test]
    fn off_subnet_traffic_is_addressed_to_the_gateway() {
        let mut a = Stack::new(cfg(1));
        a.ping(0, Ipv4Addr::new(8, 8, 8, 8), 1, 1, 8).unwrap();
        // The thing being ARPed for is the gateway, not 8.8.8.8.
        assert_eq!(arp_of(&a.pop_tx().unwrap()).target_ip, GW);
    }

    #[test]
    fn no_gateway_means_no_route_off_subnet() {
        let mut c = cfg(1);
        c.gateway = None;
        let mut a = Stack::new(c);
        assert_eq!(a.ping(0, Ipv4Addr::new(8, 8, 8, 8), 1, 1, 8), Err(SendError::NoRoute));
        assert!(a.ping(0, Ipv4Addr::new(10, 0, 0, 9), 1, 1, 8).is_ok()); // on-subnet is fine
    }

    #[test]
    fn loopback_never_touches_the_wire() {
        let mut a = Stack::new(cfg(1));
        a.ping(0, Ipv4Addr::LOCALHOST, 7, 1, 16).unwrap();
        assert!(a.pop_tx().is_none());
        let r = a.take_echo_reply(7, 1).expect("loopback echo reply");
        assert_eq!(r.from, Ipv4Addr::LOCALHOST);
        // Pinging our own address is loopback too.
        a.ping(0, a.config().ip, 7, 2, 16).unwrap();
        assert!(a.pop_tx().is_none());
        assert!(a.take_echo_reply(7, 2).is_some());
    }

    #[test]
    fn udp_delivery_to_a_bound_port() {
        let (mut a, mut b) = pair();
        b.bind_udp(7).unwrap();
        a.send_udp(0, b.config().ip, 5000, 7, b"echo me").unwrap();
        exchange(&mut a, &mut b, 0);
        let d = b.recv_udp(7).expect("datagram");
        assert_eq!((d.src, d.src_port, d.data.as_slice()), (a.config().ip, 5000, &b"echo me"[..]));
        assert!(b.recv_udp(7).is_none());
    }

    #[test]
    fn udp_to_a_closed_port_gets_icmp_port_unreachable() {
        let (mut a, mut b) = pair();
        a.send_udp(0, b.config().ip, 5000, 9999, b"hi").unwrap();
        // Resolve ARP and deliver the datagram, then look at what B sends back.
        for _ in 0..3 {
            while let Some(f) = a.pop_tx() {
                b.handle_frame(0, &f);
            }
            if let Some(f) = b.pop_tx() {
                let eth = ethernet::parse(&f).unwrap();
                if eth.ethertype == ethernet::ETHERTYPE_ARP {
                    a.handle_frame(0, &f);
                    continue;
                }
                let (h, payload) = ipv4::parse(eth.payload).unwrap();
                assert_eq!(h.protocol, ipv4::PROTO_ICMP);
                let m = icmp::parse(payload).unwrap();
                assert_eq!((m.ty, m.code), (icmp::TYPE_DEST_UNREACHABLE, icmp::CODE_PORT_UNREACHABLE));
                // The quote is the original IP header + 8 bytes: enough
                // to recover the UDP ports it concerned.
                assert_eq!(m.data.len(), 20 + 8);
                assert_eq!(u16::from_be_bytes([m.data[22], m.data[23]]), 9999);
                assert_eq!(b.stats().udp_no_port, 1);
                return;
            }
        }
        panic!("no ICMP port-unreachable produced");
    }

    #[test]
    fn udp_bind_rules_and_unbind() {
        let mut s = Stack::new(cfg(1));
        assert_eq!(s.bind_udp(0), Err(BindError::InvalidPort));
        s.bind_udp(53).unwrap();
        assert_eq!(s.bind_udp(53), Err(BindError::InUse));
        s.unbind_udp(53);
        assert!(s.bind_udp(53).is_ok());
    }

    #[test]
    fn oversize_payload_rejected_not_fragmented() {
        let mut s = Stack::new(cfg(1));
        let big = vec![0u8; 1500];
        assert_eq!(s.send_udp(0, Ipv4Addr::LOCALHOST, 1, 2, &big), Err(SendError::TooBig));
        assert!(s.send_udp(0, Ipv4Addr::LOCALHOST, 1, 2, &big[..1400]).is_ok());
    }

    #[test]
    fn arp_retries_then_gives_up_and_drops_the_queued_packet() {
        let mut a = Stack::new(cfg(1));
        a.ping(0, Ipv4Addr::new(10, 0, 0, 2), 1, 1, 8).unwrap();
        assert_eq!(a.stats().arp_requests_tx, 1);
        a.poll(500); // not due yet
        assert_eq!(a.stats().arp_requests_tx, 1);
        a.poll(1_000);
        a.poll(2_000);
        assert_eq!(a.stats().arp_requests_tx, 3); // original + 2 retries
        a.poll(3_000);
        assert_eq!(a.stats().arp_failures, 1);
        assert!(a.pending.is_empty() && a.queries.is_empty());
        // A late reply must not resurrect the dropped packet.
        while a.pop_tx().is_some() {}
        let reply = arp::Packet {
            op: arp::OP_REPLY,
            sender_mac: MacAddr([2, 0, 0, 0, 0, 2]),
            sender_ip: Ipv4Addr::new(10, 0, 0, 2),
            target_mac: cfg(1).mac,
            target_ip: cfg(1).ip,
        };
        let f = ethernet::build(cfg(1).mac, reply.sender_mac, ethernet::ETHERTYPE_ARP, &arp::build(&reply));
        a.handle_frame(3_100, &f);
        assert!(a.pop_tx().is_none());
    }

    #[test]
    fn arp_entries_expire() {
        let (mut a, mut b) = pair();
        a.ping(0, b.config().ip, 1, 1, 8).unwrap();
        exchange(&mut a, &mut b, 0);
        assert_eq!(a.arp_table().len(), 1);
        a.poll(ARP_TTL_MS + 1);
        assert!(a.arp_table().is_empty());
        // ...so the next ping has to ARP again.
        a.ping(ARP_TTL_MS + 2, b.config().ip, 1, 2, 8).unwrap();
        assert_eq!(arp_of(&a.pop_tx().unwrap()).op, arp::OP_REQUEST);
    }

    /// An IPv4 frame from a peer to `s`, for the hostile-input tests.
    fn ip_frame(s: &Stack, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
        let pkt = ipv4::build(Ipv4Addr::new(10, 0, 0, 9), dst, proto, 64, 1, payload);
        ethernet::build(s.config().mac, MacAddr([2, 0, 0, 0, 0, 9]), ethernet::ETHERTYPE_IPV4, &pkt)
    }

    #[test]
    fn raw_socket_gets_a_copy_and_the_stack_still_answers() {
        let (mut a, mut b) = pair();
        let a_ip = a.config().ip;
        let b_ip = b.config().ip;
        let raw = b.open_raw(ipv4::PROTO_ICMP).unwrap();
        a.ping(0, b_ip, 1, 1, 8).unwrap();
        exchange(&mut a, &mut b, 0);

        assert!(a.take_echo_reply(1, 1).is_some(), "the stack still replied");
        let got = b.recv_raw(raw).unwrap();
        assert_eq!(got.src, a_ip);
        assert_eq!(got.packet[0] >> 4, 4, "the packet starts at the IP header");
        assert_eq!(got.packet[9], ipv4::PROTO_ICMP);
        assert_eq!(got.packet[20], icmp::TYPE_ECHO_REQUEST);
        assert_eq!(b.recv_raw(raw), None);
    }

    #[test]
    fn raw_sockets_only_see_their_protocol_and_each_gets_a_copy() {
        let mut s = Stack::new(cfg(1));
        let (one, two) = (s.open_raw(253).unwrap(), s.open_raw(253).unwrap());
        let other = s.open_raw(254).unwrap();
        s.send_raw(0, Ipv4Addr::LOCALHOST, 253, b"hi").unwrap();
        assert_eq!(s.raw_next_len(one), Some(ipv4::HEADER_LEN + 2));
        assert_eq!(s.recv_raw(two).unwrap().packet[ipv4::HEADER_LEN..], *b"hi");
        assert_eq!(s.raw_next_len(other), None);
    }

    #[test]
    fn raw_packets_exclude_ethernet_padding() {
        let mut s = Stack::new(cfg(1));
        let raw = s.open_raw(253).unwrap();
        let mut frame = ip_frame(&s, s.config().ip, 253, b"x");
        frame.resize(frame.len() + 40, 0xaa);
        s.handle_frame(0, &frame);
        assert_eq!(s.recv_raw(raw).unwrap().packet.len(), ipv4::HEADER_LEN + 1);
    }

    #[test]
    fn raw_packets_with_a_bad_payload_checksum_are_still_delivered() {
        let mut s = Stack::new(cfg(1));
        let raw = s.open_raw(ipv4::PROTO_ICMP).unwrap();
        let frame = ip_frame(&s, s.config().ip, ipv4::PROTO_ICMP, &[8, 0, 0, 0, 1, 2, 3, 4]);
        s.handle_frame(0, &frame);
        assert!(s.recv_raw(raw).is_some());
        assert_eq!(s.stats().icmp_echo_requests_rx, 0, "but the stack ignores it");
    }

    #[test]
    fn raw_queue_is_bounded_and_counts_as_delivery() {
        let mut s = Stack::new(cfg(1));
        let raw = s.open_raw(253).unwrap();
        for _ in 0..MAX_QUEUE + 5 {
            s.send_raw(0, Ipv4Addr::LOCALHOST, 253, b"x").unwrap();
        }
        assert_eq!(s.stats().ip_dropped, 0);
        let mut n = 0;
        while s.recv_raw(raw).is_some() {
            n += 1;
        }
        assert_eq!(n, MAX_QUEUE);
    }

    #[test]
    fn unknown_protocol_with_no_raw_socket_is_dropped() {
        let mut s = Stack::new(cfg(1));
        s.send_raw(0, Ipv4Addr::LOCALHOST, 253, b"x").unwrap();
        assert_eq!(s.stats().ip_dropped, 1);
    }

    #[test]
    fn raw_socket_limit_and_close() {
        let mut s = Stack::new(cfg(1));
        let ids: Vec<u32> = (0..MAX_RAW_SOCKETS).map(|_| s.open_raw(253).unwrap()).collect();
        assert_eq!(s.open_raw(253), None);
        s.close_raw(ids[0]);
        s.send_raw(0, Ipv4Addr::LOCALHOST, 253, b"x").unwrap();
        assert_eq!(s.recv_raw(ids[0]), None, "a closed socket hears nothing");
        assert!(s.open_raw(253).is_some());
    }

    #[test]
    fn hostile_or_misaddressed_packets_are_dropped_silently() {
        let mut s = Stack::new(cfg(1));
        let echo = icmp::build_echo(icmp::TYPE_ECHO_REQUEST, 1, 1, b"x");

        // Addressed to someone else: we're a host, not a router.
        let f = ip_frame(&s, Ipv4Addr::new(10, 0, 0, 77), ipv4::PROTO_ICMP, &echo);
        s.handle_frame(0, &f);
        // Corrupt IP header checksum.
        let mut f = ip_frame(&s, s.config().ip, ipv4::PROTO_ICMP, &echo);
        f[ethernet::HEADER_LEN + 8] ^= 0x40;
        s.handle_frame(0, &f);
        // A fragment.
        let mut pkt = ipv4::build(Ipv4Addr::new(10, 0, 0, 9), s.config().ip, ipv4::PROTO_ICMP, 64, 1, &echo);
        pkt[6] = 0x20;
        pkt[10] = 0;
        pkt[11] = 0;
        let ck = crate::checksum::checksum(&pkt[..20]);
        pkt[10..12].copy_from_slice(&ck.to_be_bytes());
        s.handle_frame(0, &ethernet::build(s.config().mac, MacAddr([2, 0, 0, 0, 0, 9]), ethernet::ETHERTYPE_IPV4, &pkt));
        // Unsupported protocol (TCP).
        let f = ip_frame(&s, s.config().ip, ipv4::PROTO_TCP, &[0; 20]);
        s.handle_frame(0, &f);
        // Frame for another MAC entirely, and a runt.
        let mut f = ip_frame(&s, s.config().ip, ipv4::PROTO_ICMP, &echo);
        f[0] = 0x02;
        f[5] = 0x55;
        s.handle_frame(0, &f);
        s.handle_frame(0, &[1, 2, 3]);

        assert!(s.pop_tx().is_none(), "nothing may be answered");
        let st = s.stats();
        assert_eq!(st.icmp_echo_requests_rx, 0);
        assert_eq!(st.ip_dropped, 4);
        assert_eq!(st.rx_dropped, 2);
    }

    #[test]
    fn echo_to_a_broadcast_address_is_not_answered() {
        let mut s = Stack::new(cfg(1));
        let echo = icmp::build_echo(icmp::TYPE_ECHO_REQUEST, 1, 1, b"x");
        for dst in [Ipv4Addr::BROADCAST, Ipv4Addr::new(10, 0, 0, 255)] {
            let pkt = ipv4::build(Ipv4Addr::new(10, 0, 0, 9), dst, ipv4::PROTO_ICMP, 64, 1, &echo);
            s.handle_frame(0, &ethernet::build(MacAddr::BROADCAST, MacAddr([2, 0, 0, 0, 0, 9]), ethernet::ETHERTYPE_IPV4, &pkt));
        }
        assert!(s.pop_tx().is_none());
        assert_eq!(s.stats().icmp_echo_requests_rx, 0);
    }

    #[test]
    fn set_addr_flushes_stale_arp_state() {
        let (mut a, mut b) = pair();
        a.ping(0, b.config().ip, 1, 1, 8).unwrap();
        exchange(&mut a, &mut b, 0);
        assert!(!a.arp_table().is_empty());
        a.set_addr(Ipv4Addr::new(192, 168, 1, 5), NET, None);
        assert!(a.arp_table().is_empty());
        assert_eq!(a.config().ip, Ipv4Addr::new(192, 168, 1, 5));
    }

    /// A NetDevice backed by in-memory queues.
    struct MockDev {
        mac: MacAddr,
        rx: StdDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl NetDevice for MockDev {
        fn mac(&self) -> MacAddr {
            self.mac
        }
        fn send(&mut self, frame: &[u8]) -> Result<(), DeviceError> {
            self.sent.push(frame.to_vec());
            Ok(())
        }
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let f = self.rx.pop_front()?;
            buf[..f.len()].copy_from_slice(&f);
            Some(f.len())
        }
    }

    #[test]
    fn pump_moves_frames_between_device_and_stack() {
        let mut s = Stack::new(cfg(1));
        let mut dev = MockDev { mac: s.config().mac, rx: StdDeque::new(), sent: Vec::new() };
        s.ping(0, Ipv4Addr::new(10, 0, 0, 2), 1, 1, 8).unwrap();
        s.pump(0, &mut dev);
        assert_eq!(dev.sent.len(), 1); // the ARP request went out
        assert_eq!(s.stats().tx_frames, 1);

        // ARP reply arrives via the device; the next pump flushes the echo.
        let reply = arp::Packet {
            op: arp::OP_REPLY,
            sender_mac: MacAddr([2, 0, 0, 0, 0, 2]),
            sender_ip: Ipv4Addr::new(10, 0, 0, 2),
            target_mac: cfg(1).mac,
            target_ip: cfg(1).ip,
        };
        dev.rx.push_back(ethernet::build(cfg(1).mac, reply.sender_mac, ethernet::ETHERTYPE_ARP, &arp::build(&reply)));
        s.pump(1, &mut dev);
        assert_eq!(dev.sent.len(), 2);
        assert_eq!(ethernet::parse(&dev.sent[1]).unwrap().ethertype, ethernet::ETHERTYPE_IPV4);
    }
}
