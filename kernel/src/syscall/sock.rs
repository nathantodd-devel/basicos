//! BSD sockets over the `basicos-net` stack: `AF_INET` + `SOCK_DGRAM`
//! (UDP) or `SOCK_RAW`. There is no TCP, so `SOCK_STREAM` is refused
//! (`ESOCKTNOSUPPORT`) and `listen`/`accept` are `EOPNOTSUPP`.
//!
//! A raw socket works as on Linux: it's opened for one IP protocol, each
//! packet of that protocol arrives whole (IP header first) as well as being
//! handled by the stack as usual, and what's sent is the payload, including
//! any checksum it needs (ICMP's, say). The kernel writes the IP header, so
//! there's no `IP_HDRINCL` and no sending from someone else's address, which
//! is also why protocol 255 (`IPPROTO_RAW`, send-only with your own header)
//! is refused. This kernel has no users, so opening one needs no privilege.
//!
//! A socket is a `fd::Udp` or `fd::Raw` inside an ordinary open file
//! description, so `read`/`write`/`close`/`dup`/`poll`/`fork` work on it like
//! any fd.
//! Blocking calls loop on the stack (which pumps the NIC each time they
//! look), yielding between polls.

use super::fd::{self, Handle, Object, Raw, Udp};
use super::{errno, open_flags as of, uaccess};
use core::net::Ipv4Addr;

const AF_UNSPEC: u64 = 0;
const AF_INET: u64 = 2;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const IPPROTO_UDP: u64 = 17;
const MSG_DONTWAIT: u64 = 0x40;

const SOL_SOCKET: u64 = 1;
const SO_REUSEADDR: u64 = 2;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_BROADCAST: u64 = 6;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_KEEPALIVE: u64 = 9;
const SO_RCVTIMEO: u64 = 20;
const SO_SNDTIMEO: u64 = 21;

const ENETDOWN: i64 = -100;
const SOCKADDR_IN_LEN: usize = 16;
const EPHEMERAL_START: u16 = 49152;

static NEXT_EPHEMERAL: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(EPHEMERAL_START);

fn parse_sockaddr(ptr: u64, len: u64) -> Result<(Ipv4Addr, u16), i64> {
    if (len as usize) < SOCKADDR_IN_LEN {
        return Err(errno::EINVAL);
    }
    let raw = uaccess::slice(ptr, SOCKADDR_IN_LEN)?;
    if u16::from_le_bytes([raw[0], raw[1]]) as u64 != AF_INET {
        return Err(errno::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([raw[2], raw[3]]);
    Ok((Ipv4Addr::new(raw[4], raw[5], raw[6], raw[7]), port))
}

/// Write a `sockaddr_in` to `addr_ptr` and its length through `len_ptr`
/// (either may be null: the caller didn't want it).
fn put_sockaddr(addr_ptr: u64, len_ptr: u64, ip: Ipv4Addr, port: u16) -> Result<(), i64> {
    if addr_ptr == 0 {
        return Ok(());
    }
    let mut sa = [0u8; SOCKADDR_IN_LEN];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&ip.octets());
    let cap: u32 = if len_ptr != 0 { uaccess::get(len_ptr)? } else { SOCKADDR_IN_LEN as u32 };
    let n = (cap as usize).min(SOCKADDR_IN_LEN);
    uaccess::slice_mut(addr_ptr, n)?.copy_from_slice(&sa[..n]);
    if len_ptr != 0 {
        uaccess::put(len_ptr, SOCKADDR_IN_LEN as u32)?;
    }
    Ok(())
}

fn with_udp<R>(fd_: u64, f: impl FnOnce(&mut Udp) -> Result<R, i64>) -> Result<R, i64> {
    let h = fd::get(fd_)?;
    let mut d = h.lock();
    match &mut d.obj {
        Object::Udp(u) => f(u),
        _ => Err(errno::ENOTSOCK),
    }
}

/// `Ok` if `fd_` is a socket of either kind.
fn check_socket(fd_: u64) -> Result<(), i64> {
    let h = fd::get(fd_)?;
    let d = h.lock();
    match d.obj {
        Object::Udp(_) | Object::Raw(_) => Ok(()),
        _ => Err(errno::ENOTSOCK),
    }
}

fn is_raw(h: &Handle) -> bool {
    matches!(h.lock().obj, Object::Raw(_))
}

fn own_ip() -> Ipv4Addr {
    crate::net::config().map(|c| c.ip).unwrap_or(Ipv4Addr::UNSPECIFIED)
}

/// Bind `u` to a free ephemeral port.
fn autobind(u: &mut Udp) -> Result<(), i64> {
    use core::sync::atomic::Ordering::Relaxed;
    for _ in 0..(65535 - EPHEMERAL_START as u32) {
        let p = NEXT_EPHEMERAL.fetch_add(1, Relaxed);
        let port = if p < EPHEMERAL_START { EPHEMERAL_START } else { p };
        if port == 65535 {
            NEXT_EPHEMERAL.store(EPHEMERAL_START, Relaxed);
        }
        match crate::net::with_stack(|s| s.bind_udp(port)) {
            None => return Err(ENETDOWN),
            Some(Ok(())) => {
                u.port = port;
                return Ok(());
            }
            Some(Err(_)) => continue,
        }
    }
    Err(errno::EADDRINUSE)
}

// --- called from fd.rs -----------------------------------------------------

pub fn has_data(port: u16) -> bool {
    crate::net::with_stack(|s| s.udp_next_len(port).is_some()).unwrap_or(false)
}

pub fn pending_len(port: u16) -> usize {
    crate::net::with_stack(|s| s.udp_next_len(port)).flatten().unwrap_or(0)
}

/// Size of the next packet waiting on raw socket `id` (0 if none).
pub fn raw_pending(id: u32) -> usize {
    crate::net::with_stack(|s| s.raw_next_len(id)).flatten().unwrap_or(0)
}

fn recv_raw_into(h: &Handle, buf: &mut [u8], flags: u64) -> Result<(usize, Option<(Ipv4Addr, u16)>), i64> {
    let (id, peer, timeout, nonblock) = {
        let d = h.lock();
        let Object::Raw(r) = &d.obj else { return Err(errno::ENOTSOCK) };
        (r.id, r.peer, r.rcv_timeout_ms, d.flags & of::O_NONBLOCK != 0)
    };
    let got = fd::wait_for(nonblock || flags & MSG_DONTWAIT != 0, timeout, || {
        crate::net::with_stack(|s| loop {
            match s.recv_raw(id) {
                // A connected socket only hears from its peer.
                Some(p) if peer.is_some_and(|ip| ip != p.src) => continue,
                other => break other,
            }
        })
        .flatten()
    })?;
    let n = buf.len().min(got.packet.len());
    buf[..n].copy_from_slice(&got.packet[..n]);
    Ok((n, Some((got.src, 0))))
}

fn send_result(r: Option<Result<(), basicos_net::stack::SendError>>, len: usize) -> Result<i64, i64> {
    match r {
        None => Err(ENETDOWN),
        Some(Ok(())) => Ok(len as i64),
        Some(Err(basicos_net::stack::SendError::NoRoute)) => Err(errno::ENETUNREACH),
        Some(Err(basicos_net::stack::SendError::TooBig)) => Err(errno::EMSGSIZE),
        Some(Err(basicos_net::stack::SendError::QueueFull)) => Err(errno::ENOBUFS),
    }
}

/// Receive one datagram into `buf` (excess is discarded, as UDP does).
/// Returns the bytes copied and the sender.
pub fn recv_into(
    h: &Handle,
    buf: &mut [u8],
    flags: u64,
) -> Result<(usize, Option<(Ipv4Addr, u16)>), i64> {
    if is_raw(h) {
        return recv_raw_into(h, buf, flags);
    }
    let (port, peer, timeout, nonblock) = {
        let d = h.lock();
        let Object::Udp(u) = &d.obj else { return Err(errno::ENOTSOCK) };
        (u.port, u.peer, u.rcv_timeout_ms, d.flags & of::O_NONBLOCK != 0)
    };
    if port == 0 {
        return Err(errno::EINVAL); // never bound: nothing could ever arrive
    }
    let dgram = fd::wait_for(nonblock || flags & MSG_DONTWAIT != 0, timeout, || {
        crate::net::with_stack(|s| loop {
            match s.recv_udp(port) {
                // A connected socket only hears from its peer.
                Some(d) if peer.is_some_and(|p| p != (d.src, d.src_port)) => continue,
                other => break other,
            }
        })
        .flatten()
    })?;
    let n = buf.len().min(dgram.data.len());
    buf[..n].copy_from_slice(&dgram.data[..n]);
    Ok((n, Some((dgram.src, dgram.src_port))))
}

/// Send `buf` to `dest` (or the connected peer), binding an ephemeral
/// port first if the socket has none.
pub fn send_from(h: &Handle, buf: &[u8], dest: Option<(Ipv4Addr, u16)>) -> Result<i64, i64> {
    if is_raw(h) {
        let (protocol, dst) = {
            let d = h.lock();
            let Object::Raw(r) = &d.obj else { return Err(errno::ENOTSOCK) };
            (r.protocol, dest.map(|(ip, _)| ip).or(r.peer).ok_or(errno::EDESTADDRREQ)?)
        };
        let r = crate::net::with_stack(|s| s.send_raw(crate::net::now_ms(), dst, protocol, buf));
        return send_result(r, buf.len());
    }
    let (port, dst) = {
        let mut d = h.lock();
        let Object::Udp(u) = &mut d.obj else { return Err(errno::ENOTSOCK) };
        let dst = dest.or(u.peer).ok_or(errno::EDESTADDRREQ)?;
        if u.port == 0 {
            autobind(u)?;
        }
        (u.port, dst)
    };
    let r = crate::net::with_stack(|s| s.send_udp(crate::net::now_ms(), dst.0, port, dst.1, buf));
    send_result(r, buf.len())
}

// --- syscalls -------------------------------------------------------------

fn open_raw(ty: u64, proto: u64) -> i64 {
    // 0 means "any" for UDP but names nothing here, and 255 would be
    // `IPPROTO_RAW` (see the module docs).
    if proto == 0 || proto >= 255 {
        return errno::EPROTONOSUPPORT;
    }
    let protocol = proto as u8;
    let id = match crate::net::with_stack(|s| s.open_raw(protocol)) {
        None => return ENETDOWN,
        Some(None) => return errno::ENOBUFS,
        Some(Some(id)) => id,
    };
    let flags = of::O_RDWR | if ty & SOCK_NONBLOCK != 0 { of::O_NONBLOCK } else { 0 };
    let h = fd::new_handle(Object::Raw(Raw { id, protocol, peer: None, rcv_timeout_ms: 0 }), flags);
    match fd::install(h, ty & SOCK_CLOEXEC != 0) {
        Ok(n) => n as i64,
        Err(e) => e,
    }
}

pub fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    if domain != AF_INET {
        return errno::EAFNOSUPPORT;
    }
    if ty & 0xf == SOCK_RAW {
        return open_raw(ty, proto);
    }
    if ty & 0xf != SOCK_DGRAM {
        return errno::ESOCKTNOSUPPORT; // no TCP yet
    }
    if proto != 0 && proto != IPPROTO_UDP {
        return errno::EPROTONOSUPPORT;
    }
    if !crate::net::is_up() {
        return ENETDOWN;
    }
    let flags = of::O_RDWR | if ty & SOCK_NONBLOCK != 0 { of::O_NONBLOCK } else { 0 };
    let h = fd::new_handle(
        Object::Udp(Udp { port: 0, peer: None, rcv_timeout_ms: 0 }),
        flags,
    );
    match fd::install(h, ty & SOCK_CLOEXEC != 0) {
        Ok(n) => n as i64,
        Err(e) => e,
    }
}

pub fn sys_bind(fd_: u64, addr: u64, len: u64) -> i64 {
    let (ip, port) = match parse_sockaddr(addr, len) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let own = own_ip();
    if !(ip.is_unspecified() || ip == own || ip.is_loopback()) {
        return errno::EADDRNOTAVAIL;
    }
    // A raw socket has no port, and packets always carry our own address.
    if let Ok(h) = fd::get(fd_) {
        if is_raw(&h) {
            return 0;
        }
    }
    let r = with_udp(fd_, |u| {
        if u.port != 0 {
            return Err(errno::EINVAL); // already bound
        }
        if port == 0 {
            return autobind(u);
        }
        match crate::net::with_stack(|s| s.bind_udp(port)) {
            None => Err(ENETDOWN),
            Some(Ok(())) => {
                u.port = port;
                Ok(())
            }
            Some(Err(_)) => Err(errno::EADDRINUSE),
        }
    });
    r.map(|_| 0).unwrap_or_else(|e| e)
}

fn set_peer(fd_: u64, peer: Option<(Ipv4Addr, u16)>) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let mut d = h.lock();
    match &mut d.obj {
        Object::Udp(u) => {
            if peer.is_some() && u.port == 0 {
                if let Err(e) = autobind(u) {
                    return e;
                }
            }
            u.peer = peer;
            0
        }
        Object::Raw(r) => {
            r.peer = peer.map(|(ip, _)| ip);
            0
        }
        _ => errno::ENOTSOCK,
    }
}

pub fn sys_connect(fd_: u64, addr: u64, len: u64) -> i64 {
    // AF_UNSPEC dissolves the association.
    if len as usize >= 2 {
        if let Ok(raw) = uaccess::slice(addr, 2) {
            if u16::from_le_bytes([raw[0], raw[1]]) as u64 == AF_UNSPEC {
                return set_peer(fd_, None);
            }
        }
    }
    match parse_sockaddr(addr, len) {
        Ok(peer) => set_peer(fd_, Some(peer)),
        Err(e) => e,
    }
}

pub fn sys_sendto(fd_: u64, buf: u64, len: u64, _flags: u64, addr: u64, addrlen: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let data = match uaccess::slice(buf, len as usize) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let dest = if addr != 0 {
        match parse_sockaddr(addr, addrlen) {
            Ok(a) => Some(a),
            Err(e) => return e,
        }
    } else {
        None
    };
    send_from(&h, data, dest).unwrap_or_else(|e| e)
}

pub fn sys_recvfrom(fd_: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let out = match uaccess::slice_mut(buf, len as usize) {
        Ok(o) => o,
        Err(e) => return e,
    };
    match recv_into(&h, out, flags) {
        Ok((n, from)) => {
            if let Some((ip, port)) = from {
                if let Err(e) = put_sockaddr(addr, addrlen, ip, port) {
                    return e;
                }
            }
            n as i64
        }
        Err(e) => e,
    }
}

pub fn sys_getsockname(fd_: u64, addr: u64, addrlen: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let (ip, port) = match &h.lock().obj {
        Object::Udp(u) if u.port != 0 => (own_ip(), u.port),
        Object::Udp(_) => (Ipv4Addr::UNSPECIFIED, 0),
        Object::Raw(_) => (own_ip(), 0),
        _ => return errno::ENOTSOCK,
    };
    put_sockaddr(addr, addrlen, ip, port).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_getpeername(fd_: u64, addr: u64, addrlen: u64) -> i64 {
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let peer = match &h.lock().obj {
        Object::Udp(u) => u.peer,
        Object::Raw(r) => r.peer.map(|ip| (ip, 0)),
        _ => return errno::ENOTSOCK,
    };
    match peer {
        Some((ip, port)) => put_sockaddr(addr, addrlen, ip, port).map(|_| 0).unwrap_or_else(|e| e),
        None => errno::ENOTCONN,
    }
}

pub fn sys_setsockopt(fd_: u64, level: u64, opt: u64, val: u64, len: u64) -> i64 {
    if level != SOL_SOCKET {
        return errno::ENOPROTOOPT;
    }
    match opt {
        SO_RCVTIMEO => {
            // struct timeval { i64 sec; i64 usec }; zero = no timeout.
            if (len as usize) < 16 {
                return errno::EINVAL;
            }
            let (sec, usec): (i64, i64) = match (uaccess::get(val), uaccess::get(val + 8)) {
                (Ok(s), Ok(u)) => (s, u),
                (Err(e), _) | (_, Err(e)) => return e,
            };
            if sec < 0 || usec < 0 {
                return errno::EINVAL;
            }
            let ms = (sec as u64).saturating_mul(1000).saturating_add(usec as u64 / 1000);
            // A nonzero timeout that rounds to 0 ms must not mean "forever".
            let ms = if ms == 0 && (sec > 0 || usec > 0) { 1 } else { ms };
            let h = match fd::get(fd_) {
                Ok(h) => h,
                Err(e) => return e,
            };
            let mut d = h.lock();
            match &mut d.obj {
                Object::Udp(u) => u.rcv_timeout_ms = ms,
                Object::Raw(r) => r.rcv_timeout_ms = ms,
                _ => return errno::ENOTSOCK,
            }
            0
        }
        // Accepted, no effect: there is only one way this stack behaves.
        SO_REUSEADDR | SO_BROADCAST | SO_SNDBUF | SO_RCVBUF | SO_KEEPALIVE | SO_SNDTIMEO => {
            check_socket(fd_).map(|_| 0).unwrap_or_else(|e| e)
        }
        _ => errno::ENOPROTOOPT,
    }
}

pub fn sys_getsockopt(fd_: u64, level: u64, opt: u64, val: u64, len_ptr: u64) -> i64 {
    if level != SOL_SOCKET {
        return errno::ENOPROTOOPT;
    }
    let h = match fd::get(fd_) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let value: i32 = match opt {
        SO_TYPE if is_raw(&h) => SOCK_RAW as i32,
        SO_TYPE => SOCK_DGRAM as i32,
        SO_ERROR => 0,
        SO_SNDBUF | SO_RCVBUF => 65536,
        _ => return errno::ENOPROTOOPT,
    };
    if let Err(e) = check_socket(fd_) {
        return e;
    }
    let r = uaccess::put(val, value).and_then(|_| uaccess::put(len_ptr, 4u32));
    r.map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_shutdown(fd_: u64, _how: u64) -> i64 {
    check_socket(fd_).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_listen_accept(fd_: u64) -> i64 {
    match check_socket(fd_) {
        Ok(()) => errno::EOPNOTSUPP,
        Err(e) => e,
    }
}
