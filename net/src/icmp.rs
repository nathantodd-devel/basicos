//! ICMPv4 (RFC 792) — just what ping and UDP port-unreachable need.

use crate::checksum;
use alloc::vec::Vec;

pub const TYPE_ECHO_REPLY: u8 = 0;
pub const TYPE_DEST_UNREACHABLE: u8 = 3;
pub const TYPE_ECHO_REQUEST: u8 = 8;
pub const CODE_PORT_UNREACHABLE: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    pub ty: u8,
    pub code: u8,
    /// The 4 type-specific bytes after the checksum: identifier and
    /// sequence number for echo, unused for destination-unreachable.
    pub rest: [u8; 4],
    pub data: &'a [u8],
}

impl Message<'_> {
    pub fn echo_id(&self) -> u16 {
        u16::from_be_bytes([self.rest[0], self.rest[1]])
    }

    pub fn echo_seq(&self) -> u16 {
        u16::from_be_bytes([self.rest[2], self.rest[3]])
    }
}

/// Parse and checksum-verify an ICMP message (the IPv4 payload).
pub fn parse(b: &[u8]) -> Option<Message<'_>> {
    if b.len() < 8 || checksum::checksum(b) != 0 {
        return None;
    }
    Some(Message {
        ty: b[0],
        code: b[1],
        rest: [b[4], b[5], b[6], b[7]],
        data: &b[8..],
    })
}

pub fn build(ty: u8, code: u8, rest: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + data.len());
    m.push(ty);
    m.push(code);
    m.extend_from_slice(&[0, 0]); // checksum, filled below
    m.extend_from_slice(&rest);
    m.extend_from_slice(data);
    let ck = checksum::checksum(&m);
    m[2..4].copy_from_slice(&ck.to_be_bytes());
    m
}

pub fn build_echo(ty: u8, id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut rest = [0u8; 4];
    rest[0..2].copy_from_slice(&id.to_be_bytes());
    rest[2..4].copy_from_slice(&seq.to_be_bytes());
    build(ty, 0, rest, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_roundtrip() {
        let m = build_echo(TYPE_ECHO_REQUEST, 0xbeef, 7, b"abcd");
        let p = parse(&m).unwrap();
        assert_eq!((p.ty, p.code, p.echo_id(), p.echo_seq(), p.data), (8, 0, 0xbeef, 7, &b"abcd"[..]));
    }

    #[test]
    fn bad_checksum_or_short_rejected() {
        let mut m = build_echo(TYPE_ECHO_REQUEST, 1, 1, b"x");
        m[8] ^= 0xff;
        assert!(parse(&m).is_none());
        assert!(parse(&m[..7]).is_none());
    }

    #[test]
    fn odd_length_payload_checksums() {
        // Odd-length data exercises the padded trailing byte.
        let m = build_echo(TYPE_ECHO_REPLY, 1, 2, b"abc");
        assert_eq!(checksum::checksum(&m), 0);
    }
}
