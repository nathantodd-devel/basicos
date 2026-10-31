//! Time, process identity/control, system information, and `poll`.

use super::{errno, fd, now_ms, relax, uaccess};

// --- clocks -----------------------------------------------------------------

fn wall_ms() -> u64 {
    crate::drivers::rtc::wall_ms()
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;

fn clock_ms(clock: u64) -> Result<u64, i64> {
    match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => Ok(wall_ms()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            Ok(now_ms())
        }
        _ => Err(errno::EINVAL),
    }
}

pub fn sys_clock_gettime(clock: u64, ts: u64) -> i64 {
    match clock_ms(clock).and_then(|ms| uaccess::put(ts, [(ms / 1000) as i64, (ms % 1000 * 1_000_000) as i64])) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_clock_getres(clock: u64, ts: u64) -> i64 {
    if let Err(e) = clock_ms(clock) {
        return e;
    }
    if ts == 0 {
        return 0;
    }
    let tick_ns = 1_000_000_000 / crate::arch::interrupts::TIMER_HZ as i64;
    uaccess::put(ts, [0i64, tick_ns]).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_gettimeofday(tv: u64, _tz: u64) -> i64 {
    if tv == 0 {
        return 0;
    }
    let ms = wall_ms();
    uaccess::put(tv, [(ms / 1000) as i64, (ms % 1000 * 1000) as i64]).map(|_| 0).unwrap_or_else(|e| e)
}

pub fn sys_time(t: u64) -> i64 {
    let secs = (wall_ms() / 1000) as i64;
    if t != 0 {
        if let Err(e) = uaccess::put(t, secs) {
            return e;
        }
    }
    secs
}

fn sleep_until(deadline_ms: u64) {
    while now_ms() < deadline_ms {
        relax();
    }
}

fn read_timespec_ms(ptr: u64) -> Result<u64, i64> {
    let (sec, nsec): (i64, i64) = (uaccess::get(ptr)?, uaccess::get(ptr + 8)?);
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(errno::EINVAL);
    }
    // Round up: sleeping for less than asked is the wrong kind of early.
    Ok((sec as u64).saturating_mul(1000).saturating_add((nsec as u64).div_ceil(1_000_000)))
}

pub fn sys_nanosleep(req: u64, rem: u64) -> i64 {
    let ms = match read_timespec_ms(req) {
        Ok(ms) => ms,
        Err(e) => return e,
    };
    sleep_until(now_ms().saturating_add(ms));
    if rem != 0 {
        let _ = uaccess::put(rem, [0i64, 0i64]);
    }
    0
}

pub fn sys_clock_nanosleep(clock: u64, flags: u64, req: u64, rem: u64) -> i64 {
    const TIMER_ABSTIME: u64 = 1;
    let ms = match read_timespec_ms(req) {
        Ok(ms) => ms,
        Err(e) => return e,
    };
    let now = match clock_ms(clock) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let wait = if flags & TIMER_ABSTIME != 0 { ms.saturating_sub(now) } else { ms };
    sleep_until(now_ms().saturating_add(wait));
    if rem != 0 && flags & TIMER_ABSTIME == 0 {
        let _ = uaccess::put(rem, [0i64, 0i64]);
    }
    0
}

// --- identity ------------------------------------------------------------

/// getuid/geteuid/getgid/getegid: there are no users; everything is root.
pub fn sys_root_id() -> i64 {
    0
}

pub fn sys_getppid() -> i64 {
    #[cfg(feature = "elf-loader")]
    return crate::sched::current_ppid() as i64;
    #[cfg(not(feature = "elf-loader"))]
    return 0;
}

pub fn sys_uname(buf: u64) -> i64 {
    // struct utsname: six 65-byte NUL-padded fields.
    let fields = ["basicos", "basicos", env!("CARGO_PKG_VERSION"), "#1", "x86_64", "(none)"];
    let mut b = [0u8; 65 * 6];
    for (i, f) in fields.iter().enumerate() {
        b[i * 65..i * 65 + f.len()].copy_from_slice(f.as_bytes());
    }
    match uaccess::slice_mut(buf, b.len()) {
        Ok(o) => {
            o.copy_from_slice(&b);
            0
        }
        Err(e) => e,
    }
}

pub fn sys_sysinfo(buf: u64) -> i64 {
    let mut b = [0u8; 112];
    let mut set = |off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    set(0, now_ms() / 1000);
    #[cfg(feature = "frame-alloc")]
    {
        set(32, (crate::mem::frame::total_frames() * 4096) as u64);
        set(40, (crate::mem::frame::free_count() * 4096) as u64);
    }
    #[cfg(feature = "elf-loader")]
    {
        let n = crate::sched::process_count() as u16;
        b[80..82].copy_from_slice(&n.to_le_bytes());
    }
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    match uaccess::slice_mut(buf, b.len()) {
        Ok(o) => {
            o.copy_from_slice(&b);
            0
        }
        Err(e) => e,
    }
}

pub fn sys_getrandom(buf: u64, len: u64, _flags: u64) -> i64 {
    match uaccess::slice_mut(buf, len as usize) {
        Ok(o) => {
            fd::fill_random(o);
            len as i64
        }
        Err(e) => e,
    }
}

// --- kill ----------------------------------------------------------------

/// kill(pid, sig). There are no signal handlers, so a signal either
/// terminates the target (the fatal ones) or is dropped (the ones whose
/// default action is to ignore); signal 0 just tests that the pid exists.
/// Process groups (`pid <= 0`) don't exist here.
#[cfg(feature = "elf-loader")]
pub fn sys_kill(pid: u64, sig: u64) -> i64 {
    const SIGCHLD: u64 = 17;
    const SIGCONT: u64 = 18;
    const SIGURG: u64 = 23;
    const SIGWINCH: u64 = 28;
    if (pid as i64) <= 0 {
        return errno::ESRCH;
    }
    if sig > 64 {
        return errno::EINVAL;
    }
    if !crate::sched::process_exists(pid) {
        return errno::ESRCH;
    }
    if sig == 0 || matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH) {
        return 0;
    }
    if pid == 1 {
        return errno::EPERM;
    }
    match crate::sched::kill_process(pid, 128 + sig as i32) {
        Ok(()) => 0,
        Err(()) => errno::ESRCH, // already a zombie
    }
}

// --- poll ----------------------------------------------------------------

const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// poll(fds, nfds, timeout_ms): wait for any of the fds to become
/// readable/writable. `timeout < 0` waits forever.
pub fn sys_poll(fds: u64, nfds: u64, timeout: u64) -> i64 {
    if nfds > fd::MAX_FDS as u64 * 4 {
        return errno::EINVAL;
    }
    let timeout = timeout as i32;
    let start = now_ms();
    loop {
        let mut ready = 0i64;
        for i in 0..nfds {
            let base = fds + i * 8;
            let (fd_, events): (i32, i16) = match (uaccess::get(base), uaccess::get(base + 4)) {
                (Ok(f), Ok(e)) => (f, e),
                (Err(e), _) | (_, Err(e)) => return e,
            };
            let mut revents = 0i16;
            if fd_ >= 0 {
                match fd::get(fd_ as u64) {
                    Err(_) => revents = POLLNVAL,
                    Ok(h) => {
                        if events & POLLIN != 0 {
                            let (r, hup) = fd::ready(&h, false);
                            if r {
                                revents |= POLLIN;
                            }
                            if hup {
                                revents |= POLLHUP;
                            }
                        }
                        if events & POLLOUT != 0 {
                            let (r, hup) = fd::ready(&h, true);
                            if r {
                                revents |= POLLOUT;
                            }
                            if hup {
                                revents |= POLLHUP;
                            }
                        }
                    }
                }
            }
            if let Err(e) = uaccess::put(base + 6, revents) {
                return e;
            }
            if revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 || timeout == 0 {
            return ready;
        }
        if timeout > 0 && now_ms().saturating_sub(start) >= timeout as u64 {
            return 0;
        }
        relax();
    }
}
