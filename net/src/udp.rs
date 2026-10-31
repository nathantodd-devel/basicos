//! UDP (RFC 768).

use crate::{checksum, ipv4};
use alloc::vec::Vec;
use core::net::Ipv4Addr;

pub const HEADER_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    BadLength,
    BadChecksum,
}

/// Parse a UDP datagram (the IPv4 payload). `src`/`dst` are the IPv4
/// addresses, needed for the pseudo-header the checksum covers. A zero
/// checksum field means "sender didn't compute one" and is accepted.
pub fn parse(src: Ipv4Addr, dst: Ipv4Addr, b: &[u8]) -> Result<Datagram<'_>, ParseError> {
    if b.len() < HEADER_LEN {
        return Err(ParseError::Truncated);
    }
    let len = u16::from_be_bytes([b[4], b[5]]) as usize;
    if len < HEADER_LEN || len > b.len() {
        return Err(ParseError::BadLength);
    }
    let b = &b[..len];
    if u16::from_be_bytes([b[6], b[7]]) != 0 {
        let acc = checksum::pseudo_header_sum(src, dst, ipv4::PROTO_UDP, len as u16);
        if checksum::fold(checksum::sum(b, acc)) != 0 {
            return Err(ParseError::BadChecksum);
        }
    }
    Ok(Datagram {
        src_port: u16::from_be_bytes([b[0], b[1]]),
        dst_port: u16::from_be_bytes([b[2], b[3]]),
        data: &b[HEADER_LEN..],
    })
}

pub fn build(src: Ipv4Addr, dst: Ipv4Addr, src_port: u16, dst_port: u16, data: &[u8]) -> Vec<u8> {
    let len = (HEADER_LEN + data.len()) as u16;
    let mut d = Vec::with_capacity(len as usize);
    d.extend_from_slice(&src_port.to_be_bytes());
    d.extend_from_slice(&dst_port.to_be_bytes());
    d.extend_from_slice(&len.to_be_bytes());
    d.extend_from_slice(&[0, 0]); // checksum, filled below
    d.extend_from_slice(data);
    let acc = checksum::pseudo_header_sum(src, dst, ipv4::PROTO_UDP, len);
    let mut ck = checksum::fold(checksum::sum(&d, acc));
    if ck == 0 {
        ck = 0xffff; // 0 is reserved for "no checksum"
    }
    d[6..8].copy_from_slice(&ck.to_be_bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

    #[test]
    fn roundtrip() {
        let d = build(A, B, 1234, 7, b"ping");
        let p = parse(A, B, &d).unwrap();
        assert_eq!((p.src_port, p.dst_port, p.data), (1234, 7, &b"ping"[..]));
    }

    #[test]
    fn checksum_covers_pseudo_header() {
        let d = build(A, B, 1, 2, b"x");
        // Same bytes, but claimed to be from/to different addresses.
        assert_eq!(parse(B, A, &d).map(|_| ()), Ok(())); // swapping src/dst sums the same
        assert_eq!(
            parse(A, Ipv4Addr::new(10, 0, 0, 3), &d),
            Err(ParseError::BadChecksum)
        );
    }

    #[test]
    fn zero_checksum_accepted_and_corruption_caught() {
        let mut d = build(A, B, 1, 2, b"data");
        let mut zeroed = d.clone();
        zeroed[6] = 0;
        zeroed[7] = 0;
        assert!(parse(A, B, &zeroed).is_ok());
        d[9] ^= 0xff;
        assert_eq!(parse(A, B, &d), Err(ParseError::BadChecksum));
    }

    #[test]
    fn length_field_validated_and_trims_padding() {
        let d = build(A, B, 1, 2, b"abc");
        let mut short = d.clone();
        short[5] = 200;
        assert_eq!(parse(A, B, &short), Err(ParseError::BadLength));
        let mut padded = d.clone();
        padded.extend_from_slice(&[0; 9]);
        assert_eq!(parse(A, B, &padded).unwrap().data, b"abc");
    }
}
