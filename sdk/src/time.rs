//! Clocks and sleeping. Resolution is the kernel's timer tick (10 ms).

use core::time::Duration;

use crate::errno::{Result, check};
use crate::sys::{self, nr};

const CLOCK_REALTIME: usize = 0;
const CLOCK_MONOTONIC: usize = 1;

fn clock(id: usize) -> Result<Duration> {
    let mut ts = [0i64; 2];
    check(unsafe { sys::syscall2(nr::CLOCK_GETTIME, id, ts.as_mut_ptr() as usize) })?;
    Ok(Duration::new(ts[0] as u64, ts[1] as u32))
}

/// Time since boot; never goes backwards.
pub fn monotonic() -> Duration {
    clock(CLOCK_MONOTONIC).unwrap_or_default()
}

/// Time since the Unix epoch (from the RTC at first read, then the tick
/// counter — so it doesn't follow later RTC changes).
pub fn unix() -> Duration {
    clock(CLOCK_REALTIME).unwrap_or_default()
}

/// A point on the monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(Duration);

impl Instant {
    pub fn now() -> Instant {
        Instant(monotonic())
    }

    pub fn elapsed(&self) -> Duration {
        monotonic().saturating_sub(self.0)
    }

    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// Block for at least `d` (other processes run meanwhile).
pub fn sleep(d: Duration) {
    let ts = [d.as_secs() as i64, d.subsec_nanos() as i64];
    unsafe { sys::syscall2(nr::NANOSLEEP, ts.as_ptr() as usize, 0) };
}
