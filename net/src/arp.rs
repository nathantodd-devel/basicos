//! ARP for IPv4 over Ethernet (RFC 826).

use crate::ethernet::MacAddr;
use core::net::Ipv4Addr;

pub const LEN: usize = 28;
pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub op: u16,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
}

/// Only Ethernet/IPv4 ARP is understood; anything else is `None`.
pub fn parse(b: &[u8]) -> Option<Packet> {
    if b.len() < LEN {
        return None;
    }
    let htype = u16::from_be_bytes([b[0], b[1]]);
    let ptype = u16::from_be_bytes([b[2], b[3]]);
    if htype != 1 || ptype != 0x0800 || b[4] != 6 || b[5] != 4 {
        return None;
    }
    Some(Packet {
        op: u16::from_be_bytes([b[6], b[7]]),
        sender_mac: MacAddr(b[8..14].try_into().unwrap()),
        sender_ip: Ipv4Addr::new(b[14], b[15], b[16], b[17]),
        target_mac: MacAddr(b[18..24].try_into().unwrap()),
        target_ip: Ipv4Addr::new(b[24], b[25], b[26], b[27]),
    })
}

pub fn build(p: &Packet) -> [u8; LEN] {
    let mut b = [0u8; LEN];
    b[0..2].copy_from_slice(&1u16.to_be_bytes()); // Ethernet
    b[2..4].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4
    b[4] = 6;
    b[5] = 4;
    b[6..8].copy_from_slice(&p.op.to_be_bytes());
    b[8..14].copy_from_slice(&p.sender_mac.0);
    b[14..18].copy_from_slice(&p.sender_ip.octets());
    b[18..24].copy_from_slice(&p.target_mac.0);
    b[24..28].copy_from_slice(&p.target_ip.octets());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_rejects_non_ipv4() {
        let p = Packet {
            op: OP_REQUEST,
            sender_mac: MacAddr([2, 0, 0, 0, 0, 1]),
            sender_ip: Ipv4Addr::new(10, 0, 0, 1),
            target_mac: MacAddr::ZERO,
            target_ip: Ipv4Addr::new(10, 0, 0, 2),
        };
        let b = build(&p);
        assert_eq!(parse(&b), Some(p));
        let mut wrong = b;
        wrong[3] = 0xdd; // not IPv4
        assert_eq!(parse(&wrong), None);
        assert_eq!(parse(&b[..27]), None);
    }
}
