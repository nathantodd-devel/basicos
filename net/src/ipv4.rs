//! IPv4 (RFC 791): header parse/build. Fragmentation isn't supported —
//! fragments are recognized so the stack can drop them, and nothing is
//! ever built fragmented (the DF bit is always set).

use crate::checksum;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

pub const HEADER_LEN: usize = 20;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub protocol: u8,
    pub ttl: u8,
    pub id: u16,
    /// Header length in bytes (>= 20; options are skipped, not parsed).
    pub header_len: usize,
    /// Total datagram length from the header (excludes Ethernet padding).
    pub total_len: usize,
    /// More-fragments flag set, or a nonzero fragment offset.
    pub is_fragment: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    BadVersion,
    BadHeaderLen,
    BadLength,
    BadChecksum,
}

/// Validate and parse a packet. Returns the header and the payload, which
/// is trimmed to the header's own total length: Ethernet pads short frames
/// to 60 bytes, and that padding is not part of the datagram.
pub fn parse(pkt: &[u8]) -> Result<(Header, &[u8]), ParseError> {
    if pkt.len() < HEADER_LEN {
        return Err(ParseError::Truncated);
    }
    if pkt[0] >> 4 != 4 {
        return Err(ParseError::BadVersion);
    }
    let header_len = (pkt[0] & 0x0f) as usize * 4;
    if header_len < HEADER_LEN || header_len > pkt.len() {
        return Err(ParseError::BadHeaderLen);
    }
    let total_len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    if total_len < header_len || total_len > pkt.len() {
        return Err(ParseError::BadLength);
    }
    if checksum::checksum(&pkt[..header_len]) != 0 {
        return Err(ParseError::BadChecksum);
    }
    let frag = u16::from_be_bytes([pkt[6], pkt[7]]);
    let header = Header {
        src: Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]),
        dst: Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]),
        protocol: pkt[9],
        ttl: pkt[8],
        id: u16::from_be_bytes([pkt[4], pkt[5]]),
        header_len,
        total_len,
        is_fragment: frag & 0x2000 != 0 || frag & 0x1fff != 0,
    };
    Ok((header, &pkt[header_len..total_len]))
}

pub fn build(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    protocol: u8,
    ttl: u8,
    id: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total = (HEADER_LEN + payload.len()) as u16;
    let mut p = Vec::with_capacity(total as usize);
    p.push(0x45); // version 4, IHL 5
    p.push(0); // DSCP/ECN
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x4000u16.to_be_bytes()); // DF, offset 0
    p.push(ttl);
    p.push(protocol);
    p.extend_from_slice(&[0, 0]); // checksum, filled below
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let ck = checksum::checksum(&p[..HEADER_LEN]);
    p[10..12].copy_from_slice(&ck.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

    #[test]
    fn build_then_parse() {
        let pkt = build(A, B, PROTO_UDP, 64, 0x1234, b"hello");
        assert_eq!(pkt.len(), 25);
        let (h, payload) = parse(&pkt).unwrap();
        assert_eq!((h.src, h.dst, h.protocol, h.ttl, h.id), (A, B, PROTO_UDP, 64, 0x1234));
        assert!(!h.is_fragment);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn trims_ethernet_padding() {
        let mut pkt = build(A, B, PROTO_ICMP, 64, 1, b"hi");
        pkt.extend_from_slice(&[0; 30]); // frame padding after the datagram
        assert_eq!(parse(&pkt).unwrap().1, b"hi");
    }

    #[test]
    fn rejects_malformed() {
        let good = build(A, B, PROTO_UDP, 64, 1, b"x");
        assert_eq!(parse(&good[..19]), Err(ParseError::Truncated));

        let mut v = good.clone();
        v[0] = 0x65;
        assert_eq!(parse(&v), Err(ParseError::BadVersion));

        let mut v = good.clone();
        v[0] = 0x44; // IHL 4 -> 16 bytes
        assert_eq!(parse(&v), Err(ParseError::BadHeaderLen));

        let mut v = good.clone();
        v[3] = 200; // claims more bytes than we have
        assert_eq!(parse(&v), Err(ParseError::BadLength));

        let mut v = good.clone();
        v[8] ^= 1; // flip TTL without fixing the checksum
        assert_eq!(parse(&v), Err(ParseError::BadChecksum));
    }

    #[test]
    fn detects_fragments() {
        let mut pkt = build(A, B, PROTO_UDP, 64, 1, b"x");
        pkt[6] = 0x20; // MF, no DF
        pkt[10] = 0;
        pkt[11] = 0;
        let ck = checksum::checksum(&pkt[..20]);
        pkt[10..12].copy_from_slice(&ck.to_be_bytes());
        assert!(parse(&pkt).unwrap().0.is_fragment);
    }
}
