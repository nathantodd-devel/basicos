//! udpecho — a UDP echo server in userspace: `run C:/BIN/UDPECHO.ELF
//! [port]` (default 8000). Sends every datagram back to its sender; a
//! datagram containing exactly `quit` stops it. Driven by `make net-test`
//! to exercise the socket syscalls against the real NIC.

#![no_std]
#![no_main]

use core::net::Ipv4Addr;

use basicos_sdk::net::UdpSocket;
use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    let port = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(8000);
    let sock = match UdpSocket::bind(Ipv4Addr::UNSPECIFIED, port) {
        Ok(s) => s,
        Err(e) => {
            println!("udpecho: bind {}: {}", port, e);
            return 1;
        }
    };
    println!("udpecho: listening on udp/{}", port);
    let mut buf = [0u8; 1500];
    let mut served = 0u32;
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if &buf[..n] == b"quit" {
                    println!("udpecho: quit after {} datagrams", served);
                    return 0;
                }
                served += 1;
                let _ = sock.send_to(&buf[..n], from);
            }
            Err(e) => {
                println!("udpecho: recv: {}", e);
                return 1;
            }
        }
    }
}
