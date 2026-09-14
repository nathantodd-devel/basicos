//! Programmable Interval Timer (Intel 8253/8254), channel 0.
//!
//! On power-on the PIT fires IRQ0 at its slowest rate (~18.2 Hz, reload
//! divisor 0 = 65536). That's too coarse for the millisecond-ish
//! timestamps `log.rs` wants, so we reprogram channel 0's reload value to
//! get a faster, known rate. `interrupts.rs` still does the actual
//! counting in `TIMER_TICKS` — this just changes how often it ticks.

use super::port::outb;

const CHANNEL0_DATA: u16 = 0x40;
const COMMAND: u16 = 0x43;

/// PIT's fixed input oscillator frequency, in Hz.
const BASE_FREQUENCY: u32 = 1_193_182;

/// Channel 0, access mode lobyte/hibyte, mode 3 (square wave), binary.
const CMD_CHANNEL0_SQUARE_WAVE: u8 = 0b0011_0110;

/// Reprogram PIT channel 0 to fire at (approximately) `frequency_hz`.
///
/// The reload value is a 16-bit divisor of `BASE_FREQUENCY`, so the
/// achievable rate is `BASE_FREQUENCY / divisor` rounded down — exact for
/// the usual round numbers (100 Hz, 1000 Hz), approximate otherwise.
/// `frequency_hz` is clamped to what the hardware can represent
/// (~18.2 Hz .. 1.19 MHz).
pub unsafe fn set_frequency(frequency_hz: u32) {
    let frequency_hz = frequency_hz.max(1);
    let divisor = (BASE_FREQUENCY / frequency_hz).clamp(1, 65535) as u16;

    outb(COMMAND, CMD_CHANNEL0_SQUARE_WAVE);
    outb(CHANNEL0_DATA, (divisor & 0xFF) as u8);
    outb(CHANNEL0_DATA, (divisor >> 8) as u8);
}
