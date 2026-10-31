//! CMOS real-time clock: the wall-clock time at boot. Everything after
//! that is PIT ticks added to it (`syscall`'s `clock_gettime` and
//! friends), so this is only ever read once.

use crate::arch::x86_64::port::{inb, outb};

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

fn read(reg: u8) -> u8 {
    unsafe {
        outb(CMOS_INDEX, reg);
        inb(CMOS_DATA)
    }
}

#[derive(PartialEq, Clone, Copy)]
struct Raw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

fn read_raw() -> Raw {
    Raw {
        sec: read(0x00),
        min: read(0x02),
        hour: read(0x04),
        day: read(0x07),
        month: read(0x08),
        year: read(0x09),
    }
}

fn update_in_progress() -> bool {
    read(0x0A) & 0x80 != 0
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The RTC's current time as seconds since the Unix epoch (UTC, assuming
/// the RTC is kept in UTC, as QEMU does by default).
pub fn unix_time() -> u64 {
    // Read until two consecutive reads agree and no update is under way,
    // so we never see a half-rolled-over time.
    let mut raw = read_raw();
    for _ in 0..8 {
        while update_in_progress() {}
        let again = read_raw();
        if again == raw {
            break;
        }
        raw = again;
    }
    let status_b = read(0x0B);
    let binary = status_b & 0x04 != 0;
    let bcd = |v: u8| if binary { v } else { (v & 0x0F) + (v >> 4) * 10 };

    let pm = raw.hour & 0x80 != 0;
    let mut hour = bcd(raw.hour & 0x7F) as i64;
    if status_b & 0x02 == 0 {
        // 12-hour mode
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let century = match bcd(read(0x32)) {
        0 => 20, // register absent: assume 20xx
        c => c as i64,
    };
    let year = century * 100 + bcd(raw.year) as i64;
    let days = days_from_civil(year, bcd(raw.month) as i64, bcd(raw.day) as i64);
    (days * 86400 + hour * 3600 + bcd(raw.min) as i64 * 60 + bcd(raw.sec) as i64) as u64
}

/// Wall-clock milliseconds since the Unix epoch: the RTC as it read the
/// first time anyone asked, plus the PIT ticks since. (Resolution is the
/// timer tick, and it is never re-synced to the RTC.)
pub fn wall_ms() -> u64 {
    use crate::arch::interrupts::{TIMER_HZ, TIMER_TICKS};
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static BOOT_UNIX_MS: AtomicU64 = AtomicU64::new(0);
    let uptime_ms = TIMER_TICKS.load(Relaxed) * 1000 / TIMER_HZ as u64;
    let mut boot = BOOT_UNIX_MS.load(Relaxed);
    if boot == 0 {
        boot = (unix_time() * 1000).saturating_sub(uptime_ms);
        BOOT_UNIX_MS.store(boot, Relaxed);
    }
    boot + uptime_ms
}

pub fn wall_seconds() -> u64 {
    wall_ms() / 1000
}
