//! UDP and raw IP sockets (`AF_INET`). There is no TCP yet.
//!
//! ```ignore
//! let sock = UdpSocket::bind(Ipv4Addr::UNSPECIFIED, 9000)?;
//! let mut buf = [0u8; 1500];
//! let (n, from) = sock.recv_from(&mut buf)?;   // blocks
//! sock.send_to(&buf[..n], from)?;               // echo it back
//! ```

use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;

use crate::errno::{Errno, Result, check};
use crate::io::Fd;
use crate::sys::{self, nr};

const AF_INET: usize = 2;
const SOCK_DGRAM: usize = 2;
const SOCK_RAW: usize = 3;
const SOL_SOCKET: usize = 1;
const SO_RCVTIMEO: usize = 20;
const MSG_DONTWAIT: usize = 0x40;

fn to_sockaddr(a: SocketAddrV4) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[2..4].copy_from_slice(&a.port().to_be_bytes());
    sa[4..8].copy_from_slice(&a.ip().octets());
    sa
}

fn from_sockaddr(sa: &[u8; 16]) -> SocketAddrV4 {
    SocketAddrV4::new(
        Ipv4Addr::new(sa[4], sa[5], sa[6], sa[7]),
        u16::from_be_bytes([sa[2], sa[3]]),
    )
}

fn set_read_timeout(fd: usize, d: Option<Duration>) -> Result<()> {
    let d = d.unwrap_or_default();
    let tv = [d.as_secs() as i64, d.subsec_micros() as i64];
    check(unsafe { sys::syscall5(nr::SETSOCKOPT, fd, SOL_SOCKET, SO_RCVTIMEO, tv.as_ptr() as usize, 16) })
        .map(|_| ())
}

/// The IP protocol number of ICMP, for [`RawSocket::new`].
pub const ICMP: u8 = 1;

pub struct UdpSocket {
    fd: Fd,
}

impl UdpSocket {
    /// A socket bound to `ip:port` (`Ipv4Addr::UNSPECIFIED` for any
    /// local address; port 0 picks a free one).
    pub fn bind(ip: Ipv4Addr, port: u16) -> Result<UdpSocket> {
        let s = Self::unbound()?;
        let sa = to_sockaddr(SocketAddrV4::new(ip, port));
        check(unsafe { sys::syscall3(nr::BIND, s.fd.raw(), sa.as_ptr() as usize, sa.len()) })?;
        Ok(s)
    }

    /// A socket with no address yet; the first `send_to` gives it an
    /// ephemeral port.
    pub fn unbound() -> Result<UdpSocket> {
        let fd = check(unsafe { sys::syscall3(nr::SOCKET, AF_INET, SOCK_DGRAM, 0) })?;
        Ok(UdpSocket { fd: Fd::from_raw(fd) })
    }

    pub fn raw_fd(&self) -> usize {
        self.fd.raw()
    }

    /// Set the default peer: `send`/`recv` then talk only to it.
    pub fn connect(&self, peer: SocketAddrV4) -> Result<()> {
        let sa = to_sockaddr(peer);
        check(unsafe { sys::syscall3(nr::CONNECT, self.fd.raw(), sa.as_ptr() as usize, sa.len()) })
            .map(|_| ())
    }

    pub fn send_to(&self, buf: &[u8], to: SocketAddrV4) -> Result<usize> {
        let sa = to_sockaddr(to);
        check(unsafe {
            sys::syscall6(nr::SENDTO, self.fd.raw(), buf.as_ptr() as usize, buf.len(), 0, sa.as_ptr() as usize, sa.len())
        })
    }

    /// Send to the `connect`ed peer.
    pub fn send(&self, buf: &[u8]) -> Result<usize> {
        check(unsafe { sys::syscall6(nr::SENDTO, self.fd.raw(), buf.as_ptr() as usize, buf.len(), 0, 0, 0) })
    }

    /// Wait for a datagram. If `buf` is smaller than it, the rest is
    /// discarded.
    pub fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4)> {
        self.recv_from_flags(buf, 0)
    }

    /// Like `recv_from` but returns `EAGAIN` instead of waiting.
    pub fn try_recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4)> {
        self.recv_from_flags(buf, MSG_DONTWAIT)
    }

    fn recv_from_flags(&self, buf: &mut [u8], flags: usize) -> Result<(usize, SocketAddrV4)> {
        let mut sa = [0u8; 16];
        let mut len = 16u32;
        let n = check(unsafe {
            sys::syscall6(
                nr::RECVFROM,
                self.fd.raw(),
                buf.as_mut_ptr() as usize,
                buf.len(),
                flags,
                sa.as_mut_ptr() as usize,
                &mut len as *mut u32 as usize,
            )
        })?;
        Ok((n, from_sockaddr(&sa)))
    }

    pub fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.fd.read(buf)
    }

    pub fn local_addr(&self) -> Result<SocketAddrV4> {
        let mut sa = [0u8; 16];
        let mut len = 16u32;
        check(unsafe {
            sys::syscall3(nr::GETSOCKNAME, self.fd.raw(), sa.as_mut_ptr() as usize, &mut len as *mut u32 as usize)
        })?;
        Ok(from_sockaddr(&sa))
    }

    /// How long `recv*` waits before failing with `EAGAIN`; `None` waits
    /// forever.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> Result<()> {
        set_read_timeout(self.fd.raw(), d)
    }

    pub fn set_nonblocking(&self, on: bool) -> Result<()> {
        self.fd.set_nonblocking(on)
    }
}

/// Convenience: is this error "nothing arrived (yet)"?
pub fn would_block(e: Errno) -> bool {
    e == Errno::EAGAIN
}

/// A raw IP socket for one protocol (`ICMP`, or any other number from 1 to
/// 254). Every packet of that protocol arrives whole, IP header first, and
/// the kernel handles it as usual too. What you send is the IP payload,
/// checksum included if the protocol has one (see [`checksum`]); the kernel
/// writes the IP header.
///
/// ```ignore
/// let sock = RawSocket::new(net::ICMP)?;
/// sock.send_to(&echo_request, gateway)?;
/// let (n, from) = sock.recv_from(&mut buf)?;
/// let icmp = &buf[net::ip_header_len(&buf)..n];
/// ```
pub struct RawSocket {
    fd: Fd,
}

impl RawSocket {
    pub fn new(protocol: u8) -> Result<RawSocket> {
        let fd = check(unsafe { sys::syscall3(nr::SOCKET, AF_INET, SOCK_RAW, protocol as usize) })?;
        Ok(RawSocket { fd: Fd::from_raw(fd) })
    }

    pub fn raw_fd(&self) -> usize {
        self.fd.raw()
    }

    /// Set the default destination: `send` then goes there, and only
    /// packets from it arrive.
    pub fn connect(&self, peer: Ipv4Addr) -> Result<()> {
        let sa = to_sockaddr(SocketAddrV4::new(peer, 0));
        check(unsafe { sys::syscall3(nr::CONNECT, self.fd.raw(), sa.as_ptr() as usize, sa.len()) })
            .map(|_| ())
    }

    pub fn send_to(&self, buf: &[u8], to: Ipv4Addr) -> Result<usize> {
        let sa = to_sockaddr(SocketAddrV4::new(to, 0));
        check(unsafe {
            sys::syscall6(nr::SENDTO, self.fd.raw(), buf.as_ptr() as usize, buf.len(), 0, sa.as_ptr() as usize, sa.len())
        })
    }

    /// Send to the `connect`ed peer.
    pub fn send(&self, buf: &[u8]) -> Result<usize> {
        check(unsafe { sys::syscall6(nr::SENDTO, self.fd.raw(), buf.as_ptr() as usize, buf.len(), 0, 0, 0) })
    }

    /// Wait for a packet. If `buf` is smaller than it, the rest is discarded.
    pub fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, Ipv4Addr)> {
        let mut sa = [0u8; 16];
        let mut len = 16u32;
        let n = check(unsafe {
            sys::syscall6(
                nr::RECVFROM,
                self.fd.raw(),
                buf.as_mut_ptr() as usize,
                buf.len(),
                0,
                sa.as_mut_ptr() as usize,
                &mut len as *mut u32 as usize,
            )
        })?;
        Ok((n, *from_sockaddr(&sa).ip()))
    }

    pub fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.fd.read(buf)
    }

    /// How long `recv*` waits before failing with `EAGAIN`; `None` waits
    /// forever.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> Result<()> {
        set_read_timeout(self.fd.raw(), d)
    }

    pub fn set_nonblocking(&self, on: bool) -> Result<()> {
        self.fd.set_nonblocking(on)
    }
}

/// The length of the IP header at the start of a received raw packet.
pub fn ip_header_len(packet: &[u8]) -> usize {
    (packet[0] & 0xf) as usize * 4
}

/// The Internet checksum (RFC 1071) of `data`. To fill one in, compute it
/// with the checksum field zero; a message with a correct one sums to 0.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
