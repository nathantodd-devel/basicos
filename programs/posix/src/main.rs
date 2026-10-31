//! posix — a self-checking exercise of the kernel's POSIX-ish syscall
//! surface: files and directories, dup/fcntl, pipes, fork with shared
//! descriptors, kill, poll, devices, clocks, and UDP sockets. Prints one
//! line per failed check and a final tally; the exit status is the number
//! of failures. Driven by `make posix-test`.

#![no_std]
#![no_main]

use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;

use basicos_sdk::errno::Errno;
use basicos_sdk::fs::{self, File};
use basicos_sdk::io::{self, Fd, PollFd, POLLHUP, POLLIN, POLLOUT};
use basicos_sdk::net::{self, RawSocket, UdpSocket};
use basicos_sdk::prelude::*;
use basicos_sdk::process::{self, Fork};
use basicos_sdk::sys::{self, nr};
use basicos_sdk::time;

basicos_sdk::entry!(main);

struct Tally {
    checks: usize,
    failed: usize,
    /// The volume under test is ext4 (case-sensitive names, symlinks, ...)
    /// rather than FAT32 (8.3 uppercase names).
    ext: bool,
}

/// `base` + `/` + `rel`: a path on the volume being tested.
fn p(base: &str, rel: &str) -> String {
    format!("{}/{}", base, rel)
}

impl Tally {
    fn check(&mut self, ok: bool, what: &str) {
        self.checks += 1;
        if !ok {
            self.failed += 1;
            println!("FAIL: {}", what);
        }
    }

    fn eq<T: PartialEq + core::fmt::Debug>(&mut self, got: T, want: T, what: &str) {
        self.checks += 1;
        if got != want {
            self.failed += 1;
            println!("FAIL: {} — got {:?}, want {:?}", what, got, want);
        }
    }

    /// `r` must be an error equal to `want`.
    fn err<T: core::fmt::Debug>(&mut self, r: Result<T>, want: Errno, what: &str) {
        self.checks += 1;
        match r {
            Err(e) if e == want => {}
            other => {
                self.failed += 1;
                println!("FAIL: {} — got {:?}, want {}", what, other, want);
            }
        }
    }
}

fn read_all(f: &mut File) -> Vec<u8> {
    let mut v = Vec::new();
    f.read_to_end(&mut v).unwrap();
    v
}

fn main(args: Args) -> i32 {
    // `posix [drive]`: run everything on that drive (default `A:`, the
    // ramdisk). The filesystem type is detected from the drive itself.
    let base = args.get(1).unwrap_or("A:");
    let ext = fs::statfs(&p(base, "")).map(|s| s.magic == 0xEF53).unwrap_or(false);
    println!("posix: testing {} ({})", base, if ext { "ext4" } else { "fat32" });
    let mut t = Tally { checks: 0, failed: 0, ext };
    println!("posix: files");
    files(&mut t, base);
    println!("posix: directories");
    directories(&mut t, base);
    println!("posix: descriptors");
    descriptors(&mut t, base);
    println!("posix: pipes");
    pipes(&mut t, base);
    println!("posix: processes");
    processes(&mut t, base);
    println!("posix: devices and misc");
    devices_and_misc(&mut t, base);
    println!("posix: sockets");
    sockets(&mut t, base);
    if t.ext {
        println!("posix: ext4 extras");
        ext4_extras(&mut t, base);
    }
    println!("posix: {} checks, {} failed", t.checks, t.failed);
    if t.failed == 0 {
        println!("posix: ALL PASSED");
    }
    t.failed as i32
}

// --------------------------------------------------------------------------

fn files(t: &mut Tally, base: &str) {
    // Everything happens on A: (the writable ramdisk).
    fs::write(&p(base, "f1.txt"), b"hello world").unwrap();
    t.eq(fs::read_to_string(&p(base, "f1.txt")).unwrap().as_str(), "hello world", "write+read");
    let m = fs::metadata(&p(base, "f1.txt")).unwrap();
    t.eq(m.size, 11, "stat size");
    t.check(m.is_file() && !m.is_dir, "stat is_file");

    // Append.
    let mut f = File::append(&p(base, "f1.txt")).unwrap();
    f.write_all(b"!!").unwrap();
    f.close().unwrap();
    t.eq(fs::read_to_string(&p(base, "f1.txt")).unwrap().as_str(), "hello world!!", "O_APPEND");

    // Read/write, seek, positional I/O.
    let mut f = File::open_rw(&p(base, "f1.txt")).unwrap();
    t.eq(f.seek(6, fs::SEEK_SET).unwrap(), 6, "lseek SET");
    f.write_all(b"WORLD").unwrap();
    t.eq(f.seek(0, fs::SEEK_CUR).unwrap(), 11, "lseek CUR");
    t.eq(f.seek(-2, fs::SEEK_END).unwrap(), 11, "lseek END");
    let mut b = [0u8; 5];
    t.eq(f.read_at(&mut b, 0).unwrap(), 5, "pread len");
    t.eq(&b, b"hello", "pread data");
    t.eq(f.seek(0, fs::SEEK_CUR).unwrap(), 11, "pread keeps position");
    f.write_at(b"J", 0).unwrap();
    t.eq(f.seek(0, fs::SEEK_CUR).unwrap(), 11, "pwrite keeps position");
    f.set_len(5).unwrap();
    f.sync_all().unwrap();
    t.eq(f.metadata().unwrap().size, 5, "ftruncate + fstat");
    f.close().unwrap();
    t.eq(fs::read_to_string(&p(base, "f1.txt")).unwrap().as_str(), "Jello", "content after edits");

    // Open flags.
    t.err(
        File::open_with(&p(base, "f1.txt"), fs::O_WRONLY | fs::O_CREAT | fs::O_EXCL),
        Errno::EEXIST,
        "O_EXCL on existing",
    );
    t.err(File::open(&p(base, "nope.txt")), Errno::ENOENT, "open missing");
    let mut f = File::open(&p(base, "f1.txt")).unwrap();
    t.err(f.write(b"x"), Errno::EBADF, "write to O_RDONLY");
    drop(f);
    t.err(File::open_with("B:/HELLO.TXT", fs::O_WRONLY), Errno::EROFS, "write-open on read-only initrd");

    // Truncate by path, and a created-but-empty file is visible at once.
    fs::truncate(&p(base, "f1.txt"), 2).unwrap();
    t.eq(fs::read_to_string(&p(base, "f1.txt")).unwrap().as_str(), "Je", "truncate(path)");
    let f = File::create(&p(base, "empty.txt")).unwrap();
    t.check(fs::exists(&p(base, "empty.txt")), "creat visible before close");
    drop(f);

    // rename / unlink.
    fs::rename(&p(base, "f1.txt"), &p(base, "f2.txt")).unwrap();
    t.check(!fs::exists(&p(base, "f1.txt")) && fs::exists(&p(base, "f2.txt")), "rename file");
    fs::remove_file(&p(base, "f2.txt")).unwrap();
    fs::remove_file(&p(base, "empty.txt")).unwrap();
    t.err(fs::remove_file(&p(base, "f2.txt")), Errno::ENOENT, "unlink missing");
    if !t.ext {
        t.err(fs::write(&p(base, "waytoolongname.txt"), b""), Errno::ENAMETOOLONG, "8.3 limit");
    }
}

fn directories(t: &mut Tally, base: &str) {
    fs::create_dir(&p(base, "d1")).unwrap();
    t.err(fs::create_dir(&p(base, "d1")), Errno::EEXIST, "mkdir existing");
    t.check(fs::metadata(&p(base, "d1")).unwrap().is_dir, "stat dir");

    // chdir + relative paths + "..".
    fs::set_current_dir(&p(base, "d1")).unwrap();
    t.eq(fs::current_dir().unwrap().as_str(), &p(base, "d1"), "getcwd after chdir");
    fs::write("x.txt", b"rel").unwrap();
    fs::create_dir("sub").unwrap();
    t.eq(fs::read_to_string(&p(base, "d1/x.txt")).unwrap().as_str(), "rel", "relative create");
    t.eq(fs::read_to_string("./sub/../x.txt").unwrap().as_str(), "rel", "dot/dotdot resolution");
    fs::set_current_dir("sub").unwrap();
    t.eq(fs::current_dir().unwrap().as_str(), &p(base, "d1/sub"), "nested cwd");
    fs::set_current_dir("../..").unwrap();
    t.eq(fs::current_dir().unwrap().as_str(), &p(base, ""), "cwd at drive root");
    t.err(fs::set_current_dir(&p(base, "d1/x.txt")), Errno::ENOTDIR, "chdir into file");
    t.err(fs::set_current_dir(&p(base, "zzz")), Errno::ENOENT, "chdir missing");

    // Listing.
    let mut names: Vec<String> = fs::read_dir(&p(base, "d1")).unwrap().into_iter().map(|e| e.name).collect();
    // FAT32 stores 8.3 names uppercase; ext4 keeps them as written.
    let mut names: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    names.sort();
    t.eq(names, vec![String::from("sub"), String::from("x.txt")], "read_dir");

    // rmdir rules, rename of a directory.
    t.err(fs::remove_dir(&p(base, "d1")), Errno::ENOTEMPTY, "rmdir non-empty");
    fs::rename(&p(base, "d1/sub"), &p(base, "sub2")).unwrap();
    t.check(fs::metadata(&p(base, "sub2")).unwrap().is_dir, "rename dir across dirs");
    fs::remove_dir(&p(base, "sub2")).unwrap();
    fs::remove_file(&p(base, "d1/x.txt")).unwrap();
    fs::remove_dir(&p(base, "d1")).unwrap();
    t.check(!fs::exists(&p(base, "d1")), "rmdir");
    t.err(fs::remove_dir(&p(base, "")), Errno::EBUSY, "rmdir root");

    // Cross-drive rename.
    fs::write(&p(base, "mv.txt"), b"m").unwrap();
    t.err(fs::rename(&p(base, "mv.txt"), "C:/mv.txt"), Errno::EXDEV, "cross-drive rename");
    fs::remove_file(&p(base, "mv.txt")).unwrap();
    // cwd back to the default for the rest.
    fs::set_current_dir("C:/").unwrap();
}

fn descriptors(t: &mut Tally, base: &str) {
    fs::write(&p(base, "dup.txt"), b"0123456789").unwrap();
    let f = File::open(&p(base, "dup.txt")).unwrap();
    let a = Fd::from_raw(f.fd());
    let b = a.dup().unwrap();
    let mut buf = [0u8; 4];
    t.eq(a.read(&mut buf).unwrap(), 4, "read via original");
    t.eq(&buf, b"0123", "data via original");
    t.eq(b.read(&mut buf).unwrap(), 4, "read via dup");
    t.eq(&buf, b"4567", "dup shares the file offset");
    drop(b); // closes only the dup
    t.eq(a.read(&mut buf).unwrap(), 2, "original survives closing the dup");
    let _ = a.into_raw(); // `f` still owns the fd

    // dup2 onto a chosen number.
    let g = File::open(&p(base, "dup.txt")).unwrap();
    let gfd = Fd::from_raw(g.fd());
    gfd.dup_onto(20).unwrap();
    let _ = gfd.into_raw(); // `g` still owns it
    let fd20 = Fd::from_raw(20);
    let mut b1 = [0u8; 3];
    let n = fd20.read(&mut b1).unwrap();
    t.eq((n, &b1), (3, b"012"), "dup2 target readable");
    let _ = fd20.into_raw(); // closed explicitly below
    t.eq(unsafe { sys::syscall1(nr::CLOSE, 20) }, 0, "close dup2'd fd");
    t.eq(unsafe { sys::syscall1(nr::CLOSE, 20) }, -9, "double close is EBADF");
    drop(g);
    drop(f);
    fs::remove_file(&p(base, "dup.txt")).unwrap();

    // Bad descriptors and bad pointers.
    let mut buf = [0u8; 4];
    t.eq(unsafe { sys::syscall3(nr::READ, 63, buf.as_mut_ptr() as usize, 4) }, -9, "read bad fd");
    t.eq(unsafe { sys::syscall3(nr::READ, 9999, buf.as_mut_ptr() as usize, 4) }, -9, "read huge fd");
    t.eq(unsafe { sys::syscall3(nr::WRITE, 1, 0x1000, 4) }, -14, "write from unmapped memory is EFAULT");
    t.eq(unsafe { sys::syscall3(nr::WRITE, 1, 0xffff_ffff_ffff_0000, 8) }, -14, "write from kernel-half pointer");
    t.eq(unsafe { sys::syscall2(nr::STAT, 0x10, 0x20) }, -14, "stat with bad pointers");
    t.eq(unsafe { sys::syscall3(nr::WRITE, 1, 0, 0) }, 0, "zero-length write with NULL is fine");

    // The standard streams start out as the console.
    let out = Fd::from_raw(1);
    t.check(out.is_terminal(), "stdout is a terminal");
    let _ = out.into_raw(); // don't close stdout!
}

fn pipes(t: &mut Tally, base: &str) {
    let (r, w) = io::pipe().unwrap();
    w.write_all(b"ping").unwrap();
    let mut buf = [0u8; 16];
    t.eq(r.read(&mut buf).unwrap(), 4, "pipe read length");
    t.eq(&buf[..4], b"ping", "pipe data");

    // poll: nothing to read yet, writable.
    let mut fds = [PollFd { fd: r.raw() as i32, events: POLLIN, revents: 0 }, PollFd { fd: w.raw() as i32, events: POLLOUT, revents: 0 }];
    t.eq(io::poll(&mut fds, 0).unwrap(), 1, "poll: only the write end is ready");
    t.check(fds[0].revents == 0 && fds[1].revents & POLLOUT != 0, "poll revents");
    w.write_all(b"x").unwrap();
    t.eq(io::poll(&mut fds[..1], 0).unwrap(), 1, "poll: readable after a write");

    // Non-blocking read on an empty pipe.
    r.read(&mut buf).unwrap();
    r.set_nonblocking(true).unwrap();
    t.err(r.read(&mut buf), Errno::EAGAIN, "nonblocking read of empty pipe");
    r.set_nonblocking(false).unwrap();

    // EOF once the write end is closed; EPIPE once the read end is.
    w.write_all(b"tail").unwrap();
    w.close().unwrap();
    t.eq(r.read(&mut buf).unwrap(), 4, "drain after writer closed");
    t.eq(r.read(&mut buf).unwrap(), 0, "EOF after writer closed");
    let mut fds = [PollFd { fd: r.raw() as i32, events: POLLIN, revents: 0 }];
    io::poll(&mut fds, 0).unwrap();
    t.check(fds[0].revents & POLLHUP != 0, "poll reports HUP");
    let (r2, w2) = io::pipe().unwrap();
    r2.close().unwrap();
    t.err(w2.write(b"x"), Errno::EPIPE, "write with no readers");

    // Large transfer through a small pipe needs the reader to keep up:
    // do it across a fork below (processes()).
}

fn processes(t: &mut Tally, base: &str) {
    let me = process::getpid();

    // Child writes into a pipe; the parent reads it and reaps the child.
    let (r, w) = io::pipe().unwrap();
    match process::fork().unwrap() {
        Fork::Child => {
            drop(r);
            t_child_check(process::getppid() == me);
            w.write_all(b"from child").unwrap();
            process::exit(7);
        }
        Fork::Parent(child) => {
            drop(w);
            let mut buf = [0u8; 32];
            let n = r.read(&mut buf).unwrap();
            t.eq(&buf[..n], b"from child", "fork: pipe from child");
            t.eq(r.read(&mut buf).unwrap(), 0, "fork: EOF when child exits");
            let (pid, status) = process::wait(Some(child)).unwrap();
            t.eq((pid, status), (child, 7), "fork: wait status");
        }
    }

    // A large stream (bigger than the pipe buffer) needs both sides to
    // make progress.
    let (r, w) = io::pipe().unwrap();
    const TOTAL: usize = 100_000;
    match process::fork().unwrap() {
        Fork::Child => {
            drop(r);
            let chunk = [0x5au8; 1000];
            for _ in 0..TOTAL / chunk.len() {
                w.write_all(&chunk).unwrap();
            }
            process::exit(0);
        }
        Fork::Parent(child) => {
            drop(w);
            let mut got = 0usize;
            let mut buf = [0u8; 700];
            let mut all_ok = true;
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                all_ok &= buf[..n].iter().all(|&b| b == 0x5a);
                got += n;
            }
            t.eq(got, TOTAL, "fork: 100 KB through a pipe");
            t.check(all_ok, "fork: pipe data intact");
            process::wait(Some(child)).unwrap();
        }
    }

    // File offsets are shared across fork.
    fs::write(&p(base, "shared.txt"), b"abcdef").unwrap();
    let f = File::open(&p(base, "shared.txt")).unwrap();
    match process::fork().unwrap() {
        Fork::Child => {
            let mut b = [0u8; 3];
            let n = Fd::from_raw(f.fd()).read(&mut b).unwrap();
            process::exit(if n == 3 { 0 } else { 1 });
        }
        Fork::Parent(child) => {
            process::wait(Some(child)).unwrap();
            let mut b = [0u8; 3];
            let n = Fd::from_raw(f.fd()).read(&mut b).unwrap();
            t.eq((n, &b), (3, b"def"), "fork: child's read advanced the shared offset");
            core::mem::forget(Fd::from_raw(f.fd()));
        }
    }
    drop(f);
    fs::remove_file(&p(base, "shared.txt")).unwrap();

    // Redirect a child's stdout into a pipe with dup2.
    let (r, w) = io::pipe().unwrap();
    match process::fork().unwrap() {
        Fork::Child => {
            drop(r);
            w.dup_onto(1).unwrap();
            println!("captured line");
            process::exit(0);
        }
        Fork::Parent(child) => {
            drop(w);
            let mut buf = [0u8; 64];
            let n = r.read(&mut buf).unwrap();
            t.eq(&buf[..n], b"captured line\n", "fork+dup2: child's stdout captured");
            process::wait(Some(child)).unwrap();
        }
    }

    // kill: a child spinning in yield loops is terminated.
    match process::fork().unwrap() {
        Fork::Child => loop {
            process::yield_now();
        },
        Fork::Parent(child) => {
            t.check(process::kill(child, 0).is_ok(), "kill(pid, 0) sees the child");
            process::kill(child, process::SIGKILL).unwrap();
            let (pid, status) = process::wait(Some(child)).unwrap();
            t.eq((pid, status), (child, 128 + 9), "kill: SIGKILL status");
            t.err(process::kill(9999, 0), Errno::ESRCH, "kill: no such process");
        }
    }
    t.err(process::wait(None), Errno::ECHILD, "wait with no children");
}

/// Runs in a forked child, where a failed check can only be reported by
/// exiting nonzero.
fn t_child_check(ok: bool) {
    if !ok {
        println!("FAIL: child's getppid() != parent's pid");
    }
}

fn devices_and_misc(t: &mut Tally, base: &str) {
    let mut null = File::open_with("/dev/null", fs::O_RDWR).unwrap();
    t.eq(null.write(b"discarded").unwrap(), 9, "/dev/null write");
    let mut b = [1u8; 8];
    t.eq(null.read(&mut b).unwrap(), 0, "/dev/null read is EOF");
    drop(null);
    let mut z = File::open("/dev/zero").unwrap();
    let mut b = [0xffu8; 16];
    z.read(&mut b).unwrap();
    t.check(b.iter().all(|&x| x == 0), "/dev/zero");
    let mut a = [0u8; 32];
    let mut c = [0u8; 32];
    process::getrandom(&mut a).unwrap();
    process::getrandom(&mut c).unwrap();
    t.check(a != c && a != [0u8; 32], "getrandom gives varying bytes");
    File::open("/dev/urandom").unwrap().read(&mut a).unwrap();

    // Clocks.
    let t0 = time::Instant::now();
    time::sleep(Duration::from_millis(120));
    let dt = t0.elapsed();
    t.check(dt >= Duration::from_millis(100) && dt < Duration::from_millis(400), "sleep(120ms) took about that long");
    let unix = time::unix().as_secs();
    t.check(unix > 1_700_000_000, "wall clock is after 2023");

    let u = process::uname().unwrap();
    t.eq(u.sysname.as_str(), "basicos", "uname sysname");
    t.eq(u.machine.as_str(), "x86_64", "uname machine");
    t.check(process::getpid() > 0, "getpid");
}

fn sockets(t: &mut Tally, base: &str) {
    let lo = Ipv4Addr::LOCALHOST;
    let server = UdpSocket::bind(Ipv4Addr::UNSPECIFIED, 9000).unwrap();
    t.eq(server.local_addr().unwrap().port(), 9000, "getsockname port");
    t.err(UdpSocket::bind(Ipv4Addr::UNSPECIFIED, 9000).map(|_| ()), Errno::EADDRINUSE, "bind twice");

    // Loopback round trip via an unbound (auto-bound) client.
    let client = UdpSocket::unbound().unwrap();
    client.send_to(b"marco", SocketAddrV4::new(lo, 9000)).unwrap();
    let mut buf = [0u8; 64];
    let (n, from) = server.recv_from(&mut buf).unwrap();
    t.eq(&buf[..n], b"marco", "udp loopback data");
    t.check(from.port() >= 49152, "client got an ephemeral port");
    server.send_to(b"polo", from).unwrap();
    let (n, from2) = client.recv_from(&mut buf).unwrap();
    t.eq((&buf[..n], from2.port()), (&b"polo"[..], 9000), "udp reply");

    // Empty datagram, and truncation of an oversized one.
    client.send_to(b"", SocketAddrV4::new(lo, 9000)).unwrap();
    t.eq(server.recv_from(&mut buf).unwrap().0, 0, "empty datagram");
    client.send_to(&[7u8; 100], SocketAddrV4::new(lo, 9000)).unwrap();
    let mut small = [0u8; 10];
    t.eq(server.recv_from(&mut small).unwrap().0, 10, "truncated to buffer");
    t.err(server.try_recv_from(&mut small), Errno::EAGAIN, "rest of a truncated datagram is discarded");
    t.err(client.send_to(&[0u8; 1600], SocketAddrV4::new(lo, 9000)), Errno::EMSGSIZE, "datagram over the MTU");

    // Timeouts and non-blocking.
    server.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let t0 = time::Instant::now();
    t.err(server.recv_from(&mut buf), Errno::EAGAIN, "SO_RCVTIMEO expires");
    let waited = t0.elapsed();
    t.check(waited >= Duration::from_millis(80) && waited < Duration::from_millis(600), "timeout waited about 100ms");
    server.set_read_timeout(None).unwrap();
    server.set_nonblocking(true).unwrap();
    t.err(server.recv_from(&mut buf), Errno::EAGAIN, "O_NONBLOCK recv");
    server.set_nonblocking(false).unwrap();

    // connect() filters by peer; read/write work on a connected socket.
    let peer = UdpSocket::bind(lo, 9001).unwrap();
    let conn = UdpSocket::bind(lo, 9002).unwrap();
    conn.connect(SocketAddrV4::new(lo, 9001)).unwrap();
    let stranger = UdpSocket::unbound().unwrap();
    stranger.send_to(b"ignore me", SocketAddrV4::new(lo, 9002)).unwrap();
    peer.send_to(b"from peer", SocketAddrV4::new(lo, 9002)).unwrap();
    conn.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let n = conn.recv(&mut buf).unwrap();
    t.eq(&buf[..n], b"from peer", "connected socket only hears its peer");
    conn.send(b"reply").unwrap();
    let (n, _) = peer.recv_from(&mut buf).unwrap();
    t.eq(&buf[..n], b"reply", "send() to connected peer");

    // poll on a socket.
    let mut fds = [PollFd { fd: server.raw_fd() as i32, events: POLLIN, revents: 0 }];
    t.eq(io::poll(&mut fds, 0).unwrap(), 0, "poll: socket idle");
    client.send_to(b"z", SocketAddrV4::new(lo, 9000)).unwrap();
    t.eq(io::poll(&mut fds, 100).unwrap(), 1, "poll: socket readable");
    server.recv_from(&mut buf).unwrap();

    // Sockets close with their fd: the port becomes free again.
    drop(server);
    UdpSocket::bind(Ipv4Addr::UNSPECIFIED, 9000).map(|_| ()).unwrap();

    // Unsupported socket types are refused clearly.
    let r = unsafe { sys::syscall3(nr::SOCKET, 2, 1 /* SOCK_STREAM */, 0) };
    t.eq(r, -94, "SOCK_STREAM is ESOCKTNOSUPPORT (no TCP)");
    let r = unsafe { sys::syscall3(nr::SOCKET, 10 /* AF_INET6 */, 2, 0) };
    t.eq(r, -97, "AF_INET6 is EAFNOSUPPORT");

    raw_sockets(t);
}

const PROTO_TEST: u8 = 253; // reserved for experiments (RFC 3692)

fn raw_sockets(t: &mut Tally) {
    let lo = Ipv4Addr::LOCALHOST;
    let timeout = Some(Duration::from_secs(1));
    let mut buf = [0u8; 256];

    t.err(RawSocket::new(0).map(|_| ()), Errno::EPROTONOSUPPORT, "raw: protocol 0 refused");
    t.err(RawSocket::new(255).map(|_| ()), Errno::EPROTONOSUPPORT, "raw: IPPROTO_RAW refused");

    // Each opened socket gets its own copy; others see nothing.
    let a = RawSocket::new(PROTO_TEST).unwrap();
    let b = RawSocket::new(PROTO_TEST).unwrap();
    let other = RawSocket::new(254).unwrap();
    a.set_read_timeout(timeout).unwrap();
    b.set_read_timeout(timeout).unwrap();
    other.set_nonblocking(true).unwrap();
    t.eq(a.send_to(b"hello", lo), Ok(5), "raw: send");
    let (n, from) = a.recv_from(&mut buf).unwrap();
    t.eq(from, lo, "raw: source address");
    t.eq(buf[9], PROTO_TEST, "raw: packet starts at the IP header");
    t.eq(&buf[net::ip_header_len(&buf)..n], b"hello", "raw: payload after the header");
    t.eq(b.recv(&mut buf).map(|n| n >= 25), Ok(true), "raw: second socket gets a copy");
    t.err(other.recv(&mut buf), Errno::EAGAIN, "raw: other protocols see nothing");

    // Truncation, as with UDP, and poll.
    a.send_to(b"truncated", lo).unwrap();
    let mut fds = [PollFd { fd: a.raw_fd() as i32, events: POLLIN, revents: 0 }];
    t.eq(io::poll(&mut fds, 100).unwrap(), 1, "raw: poll sees a packet");
    let mut small = [0u8; 4];
    t.eq(a.recv(&mut small), Ok(4), "raw: short read truncates");
    b.recv(&mut buf).unwrap();

    // A connected socket sends to, and only hears from, its peer.
    let connected = RawSocket::new(PROTO_TEST).unwrap();
    connected.connect(Ipv4Addr::new(10, 0, 2, 99)).unwrap();
    connected.set_nonblocking(true).unwrap();
    a.send_to(b"x", lo).unwrap();
    t.err(connected.recv(&mut buf), Errno::EAGAIN, "raw: connected socket filters by source");
    a.recv(&mut buf).unwrap();
    b.recv(&mut buf).unwrap();
    let to_lo = RawSocket::new(PROTO_TEST).unwrap();
    to_lo.connect(lo).unwrap();
    t.eq(to_lo.send(b"via connect"), Ok(11), "raw: send to the connected peer");
    let n = a.recv(&mut buf).unwrap();
    t.eq(&buf[net::ip_header_len(&buf)..n], b"via connect", "raw: arrives from the peer");

    // ICMP: the stack still answers an echo request we watch go past.
    let icmp = RawSocket::new(net::ICMP).unwrap();
    icmp.set_read_timeout(timeout).unwrap();
    let mut m = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'h', b'i'];
    let sum = net::checksum(&m);
    m[2..4].copy_from_slice(&sum.to_be_bytes());
    icmp.send_to(&m, lo).unwrap();
    let mut types = Vec::new();
    for _ in 0..2 {
        let n = icmp.recv(&mut buf).unwrap();
        let h = net::ip_header_len(&buf);
        t.eq(net::checksum(&buf[h..n]), 0, "raw: ICMP checksum");
        types.push(buf[h]);
    }
    t.eq(types, vec![8, 0], "raw: sees the request, then the stack's reply");

    // The socket follows its fd: a child exiting doesn't close the parent's
    // copy, and closing the last one frees its place.
    if let Fork::Parent(child) = process::fork().unwrap() {
        process::wait(Some(child)).unwrap();
        a.set_nonblocking(true).unwrap();
        while a.recv(&mut buf).is_ok() {} // drain what's queued
        a.set_nonblocking(false).unwrap();
        b.send_to(b"after fork", lo).unwrap();
        t.check(a.recv(&mut buf).is_ok(), "raw: still open after a child exits");
    } else {
        process::exit(0);
    }
    drop((a, b, other, connected, to_lo, icmp));
    for _ in 0..64 {
        drop(RawSocket::new(PROTO_TEST).unwrap());
    }
    t.check(true, "raw: closing frees the socket");
}

// --------------------------------------------------------------------------
// ext4-only behaviour: what a real Unix filesystem has and FAT32 doesn't
// --------------------------------------------------------------------------

fn ext4_extras(t: &mut Tally, base: &str) {
    // Long, case-sensitive names.
    let long = "n".repeat(255);
    fs::write(&p(base, &long), b"long").unwrap();
    t.eq(fs::read_to_string(&p(base, &long)).unwrap().as_str(), "long", "255-byte name");
    t.err(fs::write(&p(base, &"m".repeat(256)), b""), Errno::ENAMETOOLONG, "256-byte name");
    fs::remove_file(&p(base, &long)).unwrap();
    fs::write(&p(base, "Case.txt"), b"upper").unwrap();
    fs::write(&p(base, "case.txt"), b"lower").unwrap();
    t.eq(fs::read_to_string(&p(base, "Case.txt")).unwrap().as_str(), "upper", "names are case-sensitive (1)");
    t.eq(fs::read_to_string(&p(base, "case.txt")).unwrap().as_str(), "lower", "names are case-sensitive (2)");
    fs::remove_file(&p(base, "Case.txt")).unwrap();
    fs::remove_file(&p(base, "case.txt")).unwrap();

    // Symbolic links.
    fs::write(&p(base, "target.txt"), b"linked data").unwrap();
    fs::create_dir(&p(base, "realdir")).unwrap();
    fs::write(&p(base, "realdir/inner.txt"), b"inner").unwrap();
    fs::symlink("target.txt", &p(base, "rel_link")).unwrap();
    fs::symlink("/target.txt", &p(base, "abs_link")).unwrap();
    fs::symlink("realdir", &p(base, "dir_link")).unwrap();
    fs::symlink("nowhere", &p(base, "dangling")).unwrap();
    t.eq(fs::read_link(&p(base, "rel_link")).unwrap().as_str(), "target.txt", "readlink");
    t.eq(fs::read_to_string(&p(base, "rel_link")).unwrap().as_str(), "linked data", "read through relative link");
    t.eq(fs::read_to_string(&p(base, "abs_link")).unwrap().as_str(), "linked data", "read through absolute link");
    t.eq(fs::read_to_string(&p(base, "dir_link/inner.txt")).unwrap().as_str(), "inner", "path through a directory link");
    let lm = fs::symlink_metadata(&p(base, "rel_link")).unwrap();
    t.check(lm.is_symlink() && !lm.is_file(), "lstat sees the link");
    let sm = fs::metadata(&p(base, "rel_link")).unwrap();
    t.check(sm.is_file() && sm.size == 11, "stat follows the link");
    t.err(File::open(&p(base, "dangling")), Errno::ENOENT, "open dangling link");
    t.check(fs::symlink_metadata(&p(base, "dangling")).is_ok(), "lstat on a dangling link works");
    t.err(fs::read_link(&p(base, "target.txt")), Errno::EINVAL, "readlink on a regular file");
    fs::symlink("loop_b", &p(base, "loop_a")).unwrap();
    fs::symlink("loop_a", &p(base, "loop_b")).unwrap();
    t.err(File::open(&p(base, "loop_a")), Errno::ELOOP, "symlink loop");
    // Removing a link leaves its target alone.
    fs::remove_file(&p(base, "rel_link")).unwrap();
    t.check(fs::exists(&p(base, "target.txt")), "unlinking a symlink keeps the target");
    let names: Vec<String> = fs::read_dir(&p(base, "")).unwrap().into_iter().map(|e| e.name).collect();
    t.check(names.iter().any(|n| n == "abs_link"), "links show up in listings");
    for l in ["abs_link", "dir_link", "dangling", "loop_a", "loop_b", "target.txt", "realdir/inner.txt"] {
        fs::remove_file(&p(base, l)).unwrap();
    }
    fs::remove_dir(&p(base, "realdir")).unwrap();

    // Hard links, permissions, times.
    fs::write(&p(base, "hl1"), b"shared").unwrap();
    fs::hard_link(&p(base, "hl1"), &p(base, "hl2")).unwrap();
    t.eq(fs::metadata(&p(base, "hl1")).unwrap().nlink, 2, "nlink after link");
    fs::write(&p(base, "hl2"), b"changed").unwrap();
    t.eq(fs::read_to_string(&p(base, "hl1")).unwrap().as_str(), "changed", "hard links share data");
    fs::remove_file(&p(base, "hl1")).unwrap();
    t.eq(fs::metadata(&p(base, "hl2")).unwrap().nlink, 1, "nlink after unlink");
    fs::set_permissions(&p(base, "hl2"), 0o600).unwrap();
    t.eq(fs::metadata(&p(base, "hl2")).unwrap().permissions(), 0o600, "chmod");
    t.check(fs::metadata(&p(base, "hl2")).unwrap().mtime > 1_700_000_000, "mtime is a real timestamp");
    fs::remove_file(&p(base, "hl2")).unwrap();

    // A file that's open survives being unlinked.
    fs::write(&p(base, "ghost"), b"still here").unwrap();
    let mut f = File::open(&p(base, "ghost")).unwrap();
    fs::remove_file(&p(base, "ghost")).unwrap();
    t.check(!fs::exists(&p(base, "ghost")), "unlinked name is gone");
    t.eq(f.read_to_string().unwrap().as_str(), "still here", "open unlinked file still readable");
    drop(f);

    // Big files stream straight to the volume (no in-memory copy).
    let free0 = fs::statfs(&p(base, "")).unwrap();
    t.eq(free0.magic, 0xEF53, "statfs reports ext4");
    let mut big = File::create(&p(base, "posix_big.bin")).unwrap();
    // (Heap, not stack: a process starts with only 32 KiB of stack.)
    let mut chunk = vec![0u8; 65536];
    for i in 0..48u32 {
        for (j, b) in chunk.iter_mut().enumerate() {
            *b = (i as usize * 31 + j) as u8;
        }
        big.write_all(&chunk).unwrap();
    }
    t.eq(big.metadata().unwrap().size, 48 * 65536, "3 MiB file size");
    let free1 = fs::statfs(&p(base, "")).unwrap();
    t.check(free1.free_blocks + 700 < free0.free_blocks, "blocks were allocated");
    drop(big);
    let mut big = File::open(&p(base, "posix_big.bin")).unwrap();
    let mut ok = true;
    for i in 0..48u32 {
        let mut got = vec![0u8; 65536];
        big.read(&mut got).map(|_| ()).unwrap_or(());
        let mut n = 0;
        // (read may return short counts; top up)
        while n < got.len() {
            let r = big.read_at(&mut got[n..], i as u64 * 65536 + n as u64).unwrap();
            if r == 0 {
                break;
            }
            n += r;
        }
        ok &= got.iter().enumerate().all(|(j, &b)| b == (i as usize * 31 + j) as u8);
    }
    t.check(ok, "3 MiB file reads back intact");
    drop(big);
    fs::truncate(&p(base, "posix_big.bin"), 1 << 20).unwrap();
    t.eq(fs::metadata(&p(base, "posix_big.bin")).unwrap().size, 1 << 20, "truncate to 1 MiB");
    fs::remove_file(&p(base, "posix_big.bin")).unwrap();
    let free2 = fs::statfs(&p(base, "")).unwrap();
    t.check(free2.free_blocks + 8 >= free0.free_blocks, "space returned after delete");

    // A directory with a few hundred entries.
    fs::create_dir(&p(base, "crowd")).unwrap();
    for i in 0..300 {
        fs::write(&p(base, &format!("crowd/entry-{:04}.dat", i)), b"x").unwrap();
    }
    t.eq(fs::read_dir(&p(base, "crowd")).unwrap().len(), 300, "300 entries listed");
    for i in 0..300 {
        fs::remove_file(&p(base, &format!("crowd/entry-{:04}.dat", i))).unwrap();
    }
    fs::remove_dir(&p(base, "crowd")).unwrap();
    fs::sync();
}
