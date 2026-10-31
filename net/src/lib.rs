//! `basicos-net` — the IPv4 stack, decoupled from the kernel.
//!
//! Like `basicos-fs`, this is `#![no_std]` (plus `alloc`) and touches no
//! hardware, so the same code runs in the kernel and under `cargo test`
//! on a host. The seam is [`stack::NetDevice`]: a NIC driver implements
//! it, and [`stack::Stack`] does everything above raw Ethernet frames —
//! ARP, IPv4, ICMP echo, UDP, loopback.
//!
//! The wire-format layers ([`ethernet`], [`arp`], [`ipv4`], [`icmp`],
//! [`udp`], [`checksum`]) are plain parse/build functions; [`stack`] is
//! the state machine that ties them together. See its module docs for
//! the sans-IO design.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod arp;
pub mod checksum;
pub mod ethernet;
pub mod icmp;
pub mod ipv4;
pub mod stack;
pub mod udp;

pub use ethernet::MacAddr;
pub use stack::{Config, NetDevice, Stack};
