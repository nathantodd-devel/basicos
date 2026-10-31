//! ping <a.b.c.d> [count] — send ICMP echo requests over a raw socket, one
//! a second.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

use core::net::Ipv4Addr;
use core::time::Duration;
use net::RawSocket;

basicos_sdk::entry!(main);

const ECHO_REPLY: u8 = 0;
const ECHO_REQUEST: u8 = 8;
const TIMEOUT: Duration = Duration::from_secs(2);
const PAYLOAD: &[u8] = b"basicos ping 0123456789abcdefghi";

fn echo_request(id: u16, seq: u16) -> Vec<u8> {
    let mut m = vec![ECHO_REQUEST, 0, 0, 0];
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(PAYLOAD);
    let sum = net::checksum(&m);
    m[2..4].copy_from_slice(&sum.to_be_bytes());
    m
}

/// Whether `icmp` is a valid reply to our request `id`/`seq`.
fn is_reply(icmp: &[u8], id: u16, seq: u16) -> bool {
    icmp.len() >= 8
        && icmp[0] == ECHO_REPLY
        && net::checksum(icmp) == 0
        && icmp[4..6] == id.to_be_bytes()
        && icmp[6..8] == seq.to_be_bytes()
}

/// Wait for the reply to `seq`: `Ok(Some((bytes, ttl)))`, or `Ok(None)` if
/// it doesn't come in time. Everything else ICMP on the machine comes
/// through this socket too, so anything that isn't our reply is skipped.
fn wait_reply(sock: &RawSocket, id: u16, seq: u16, sent: time::Instant) -> Result<Option<(usize, u8)>> {
    let mut buf = [0u8; 1500];
    loop {
        let left = TIMEOUT.saturating_sub(sent.elapsed());
        if left.is_zero() {
            return Ok(None);
        }
        sock.set_read_timeout(Some(left))?;
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if net::would_block(e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let header = net::ip_header_len(&buf);
        if is_reply(&buf[header..n], id, seq) {
            return Ok(Some((n - header - 8, buf[8])));
        }
    }
}

fn ping(dst: Ipv4Addr, count: u16) -> Result<u16> {
    let sock = RawSocket::new(net::ICMP)?;
    sock.connect(dst)?;
    let id = process::getpid() as u16;

    let mut received = 0;
    for seq in 1..=count {
        let sent = time::Instant::now();
        sock.send(&echo_request(id, seq))?;
        match wait_reply(&sock, id, seq, sent)? {
            Some((bytes, ttl)) => {
                received += 1;
                let ms = sent.elapsed().as_millis();
                println!("{} bytes from {}: seq={} ttl={} time={} ms", bytes, dst, seq, ttl, ms);
            }
            None => println!("request timed out: seq={}", seq),
        }
        if seq < count {
            time::sleep(Duration::from_secs(1));
        }
    }
    Ok(received)
}

fn main(args: Args) -> i32 {
    let Some(dst) = args.get(1).and_then(|a| a.parse::<Ipv4Addr>().ok()) else {
        eprintln!("usage: ping <a.b.c.d> [count]");
        return 2;
    };
    let count = args.get(2).and_then(|n| n.parse::<u16>().ok()).unwrap_or(4).clamp(1, 100);

    match ping(dst, count) {
        Ok(received) => {
            println!("{} sent, {} received", count, received);
            if received == 0 { 1 } else { 0 }
        }
        Err(e) => {
            eprintln!("ping: {}", e);
            1
        }
    }
}
