//! Ethernet II framing.

use alloc::vec::Vec;
use core::fmt;

pub const HEADER_LEN: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: MacAddr = MacAddr([0xff; 6]);
    pub const ZERO: MacAddr = MacAddr([0; 6]);

    pub fn is_broadcast(&self) -> bool {
        *self == Self::BROADCAST
    }

    /// Group bit set (broadcast is a special case of multicast).
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 1 != 0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A parsed frame borrowing its payload from the receive buffer.
pub struct Frame<'a> {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse(b: &[u8]) -> Option<Frame<'_>> {
    if b.len() < HEADER_LEN {
        return None;
    }
    Some(Frame {
        dst: MacAddr(b[0..6].try_into().unwrap()),
        src: MacAddr(b[6..12].try_into().unwrap()),
        ethertype: u16::from_be_bytes([b[12], b[13]]),
        payload: &b[HEADER_LEN..],
    })
}

pub fn build(dst: MacAddr, src: MacAddr, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    v.extend_from_slice(&dst.0);
    v.extend_from_slice(&src.0);
    v.extend_from_slice(&ethertype.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let a = MacAddr([2, 0, 0, 0, 0, 1]);
        let b = MacAddr([2, 0, 0, 0, 0, 2]);
        let f = build(a, b, ETHERTYPE_IPV4, &[1, 2, 3]);
        assert_eq!(f.len(), 17);
        let p = parse(&f).unwrap();
        assert_eq!((p.dst, p.src, p.ethertype, p.payload), (a, b, 0x0800, &[1, 2, 3][..]));
        assert!(parse(&f[..13]).is_none());
    }

    #[test]
    fn address_classes() {
        assert!(MacAddr::BROADCAST.is_broadcast() && MacAddr::BROADCAST.is_multicast());
        assert!(MacAddr([0x01, 0, 0x5e, 0, 0, 1]).is_multicast());
        assert!(!MacAddr([2, 0, 0, 0, 0, 1]).is_multicast());
    }
}
