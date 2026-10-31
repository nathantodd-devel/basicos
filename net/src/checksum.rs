//! The Internet checksum (RFC 1071): the one's-complement sum of 16-bit
//! big-endian words, complemented. IPv4, ICMP and UDP all use it.
//!
//! Chain calls to [`sum`] to checksum discontiguous data (a UDP
//! pseudo-header followed by the datagram); only the *last* chunk may
//! have odd length, since an odd trailing byte is padded with a zero.

use core::net::Ipv4Addr;

/// Add `data`'s 16-bit words into the running total `acc`.
pub fn sum(data: &[u8], mut acc: u32) -> u32 {
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        acc += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        acc += (*b as u32) << 8;
    }
    acc
}

/// Fold the carries back in and complement.
pub fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

/// Checksum of one contiguous buffer. A buffer that *contains* a correct
/// checksum field sums to zero, which is how received packets are checked.
pub fn checksum(data: &[u8]) -> u16 {
    fold(sum(data, 0))
}

/// The running total for a TCP/UDP pseudo-header (source, destination,
/// protocol, transport-layer length).
pub fn pseudo_header_sum(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, len: u16) -> u32 {
    sum(&src.octets(), 0) + sum(&dst.octets(), 0) + protocol as u32 + len as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1071_example() {
        // The worked example in RFC 1071 section 3.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(sum(&data, 0), 0x2ddf0); // un-folded total
        assert_eq!(fold(sum(&data, 0)), 0x220d); // ~0xddf2
        assert_eq!(checksum(&data), 0x220d);
    }

    #[test]
    fn odd_length_pads_with_zero_low_byte() {
        assert_eq!(sum(&[0xab], 0), 0xab00);
        assert_eq!(sum(&[0x12, 0x34, 0x56], 0), 0x1234 + 0x5600);
    }

    #[test]
    fn valid_buffer_sums_to_zero() {
        // A real IPv4 header with a correct checksum (0xb861).
        let hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0xb8, 0x61, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(checksum(&hdr), 0);
        let mut bad = hdr;
        bad[3] ^= 1;
        assert_ne!(checksum(&bad), 0);
    }
}
