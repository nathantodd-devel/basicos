//! Kernel logging subsystem — Linux printk equivalent.
//!
//! Provides formatted logging with log levels, timestamps, ring buffer
//! storage, and multiple output backends (VGA + serial).
//!
//! # Usage
//!
//! ```ignore
//! pr_info!("Boot complete, {} CPUs detected", cpu_count);
//! pr_err!("Failed to initialize device: {:#x}", error_code);
//! pr_debug!("Entering function foo");  // Only in debug builds
//! ```

use crate::arch::interrupts::TIMER_TICKS;
use crate::drivers::serial::SerialWriter;
use crate::drivers::vga::VgaWriter;
use crate::sync::{LogRingBuffer, Spinlock};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

// ============================================================================
// Log Levels
// ============================================================================

/// Log levels matching Linux kernel conventions.
/// Lower number = higher priority.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LogLevel {
    /// System is unusable
    Emergency = 0,
    /// Action must be taken immediately
    Alert = 1,
    /// Critical conditions
    Critical = 2,
    /// Error conditions
    Error = 3,
    /// Warning conditions
    Warning = 4,
    /// Normal but significant condition
    Notice = 5,
    /// Informational
    Info = 6,
    /// Debug-level messages
    Debug = 7,
}

impl LogLevel {
    /// Get a human-readable name for this log level.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Emergency => "EMERG",
            LogLevel::Alert => "ALERT",
            LogLevel::Critical => "CRIT",
            LogLevel::Error => "ERROR",
            LogLevel::Warning => "WARN",
            LogLevel::Notice => "NOTICE",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }

    /// Get the numeric prefix (like Linux's `<N>` format).
    #[allow(dead_code)]
    pub fn prefix(self) -> &'static str {
        match self {
            LogLevel::Emergency => "<0>",
            LogLevel::Alert => "<1>",
            LogLevel::Critical => "<2>",
            LogLevel::Error => "<3>",
            LogLevel::Warning => "<4>",
            LogLevel::Notice => "<5>",
            LogLevel::Info => "<6>",
            LogLevel::Debug => "<7>",
        }
    }
}

// ============================================================================
// Global State
// ============================================================================

/// Ring buffer size: 16KB (must be power of 2)
const LOG_BUFFER_SIZE: usize = 16384;

/// Global log buffer protected by spinlock.
static LOG_BUFFER: Spinlock<LogRingBuffer<LOG_BUFFER_SIZE>> =
    Spinlock::new(LogRingBuffer::new());

/// Current console log level threshold.
/// Messages with level <= this value are displayed on console.
static CONSOLE_LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// Enable/disable VGA output.
static VGA_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enable/disable serial output.
static SERIAL_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enable/disable timestamps.
static TIMESTAMPS_ENABLED: AtomicBool = AtomicBool::new(true);

// ============================================================================
// Console Writer
// ============================================================================

/// Internal writer that dispatches to all enabled output backends
/// and stores messages in the ring buffer.
struct ConsoleWriter {
    level: LogLevel,
}

impl Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Always write to ring buffer
        {
            let mut buf = LOG_BUFFER.lock();
            buf.write(s.as_bytes());
        }

        // Only output to console if level <= threshold
        let threshold = CONSOLE_LOG_LEVEL.load(Ordering::Relaxed);
        if (self.level as u8) <= threshold {
            if VGA_ENABLED.load(Ordering::Relaxed) {
                let _ = VgaWriter.write_str(s);
            }
            if SERIAL_ENABLED.load(Ordering::Relaxed) {
                let _ = SerialWriter.write_str(s);
            }
        }
        Ok(())
    }
}

/// Raw writer for kprint!/kprintln! (no level filtering).
#[allow(dead_code)]
struct RawWriter;

impl Write for RawWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Write to ring buffer
        {
            let mut buf = LOG_BUFFER.lock();
            buf.write(s.as_bytes());
        }

        // Write to all backends unconditionally
        if VGA_ENABLED.load(Ordering::Relaxed) {
            let _ = VgaWriter.write_str(s);
        }
        if SERIAL_ENABLED.load(Ordering::Relaxed) {
            let _ = SerialWriter.write_str(s);
        }
        Ok(())
    }
}

// ============================================================================
// Core Print Functions
// ============================================================================

/// Print with log level, formatting, and optional timestamp.
/// Used by pr_info!, pr_err!, etc.
///
/// Runs with interrupts disabled for its whole duration, not just each
/// individual write: with preemptive scheduling (the `scheduler`
/// feature), a log line is built from several separate writes
/// (timestamp, level prefix, message), and without this a preempting
/// task's own output could land in the middle of ours, interleaving
/// mid-line instead of printing as a clean, whole line.
#[doc(hidden)]
pub fn _kprint_with_level(level: LogLevel, args: fmt::Arguments) {
    let was_enabled = crate::arch::interrupts::disable();

    let mut writer = ConsoleWriter { level };

    // Timestamp: [seconds.milliseconds]
    if TIMESTAMPS_ENABLED.load(Ordering::Relaxed) {
        let ticks = TIMER_TICKS.load(Ordering::Relaxed);
        let hz = crate::arch::interrupts::TIMER_HZ as u64;
        let secs = ticks / hz;
        let ms = (ticks % hz) * 1000 / hz;
        let _ = write!(writer, "[{:5}.{:03}] ", secs, ms);
    }

    // Log level prefix
    let _ = write!(writer, "{}: ", level.as_str());

    // Actual message
    let _ = writer.write_fmt(args);

    crate::arch::interrupts::restore(was_enabled);
}

/// Print without level prefix (raw output).
/// Used by kprint!/kprintln!
///
/// Same interrupts-disabled-for-the-whole-call treatment as
/// `_kprint_with_level`, for the same reason.
#[doc(hidden)]
pub fn _kprint(args: fmt::Arguments) {
    let was_enabled = crate::arch::interrupts::disable();
    let mut writer = RawWriter;
    let _ = writer.write_fmt(args);
    crate::arch::interrupts::restore(was_enabled);
}

// ============================================================================
// Configuration Functions
// ============================================================================

/// Set the console log level threshold.
/// Messages with level <= threshold are displayed.
#[allow(dead_code)]
pub fn set_console_log_level(level: LogLevel) {
    CONSOLE_LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Get the current console log level threshold.
#[allow(dead_code)]
pub fn get_console_log_level() -> LogLevel {
    match CONSOLE_LOG_LEVEL.load(Ordering::Relaxed) {
        0 => LogLevel::Emergency,
        1 => LogLevel::Alert,
        2 => LogLevel::Critical,
        3 => LogLevel::Error,
        4 => LogLevel::Warning,
        5 => LogLevel::Notice,
        6 => LogLevel::Info,
        _ => LogLevel::Debug,
    }
}

/// Enable or disable VGA output.
#[allow(dead_code)]
pub fn enable_vga(enabled: bool) {
    VGA_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Enable or disable serial output.
#[allow(dead_code)]
pub fn enable_serial(enabled: bool) {
    SERIAL_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Enable or disable timestamps.
#[allow(dead_code)]
pub fn enable_timestamps(enabled: bool) {
    TIMESTAMPS_ENABLED.store(enabled, Ordering::Relaxed);
}

// ============================================================================
// Log Buffer Access (dmesg-like)
// ============================================================================

/// Read all buffered log messages, calling the callback for each byte.
/// Returns total bytes read.
pub fn read_log<F: FnMut(u8)>(f: F) -> usize {
    let buf = LOG_BUFFER.lock();
    buf.read_all(f)
}

/// Dump all buffered log messages to VGA (for debugging).
#[allow(dead_code)]
pub fn dump_log_to_vga() {
    read_log(|byte| {
        crate::drivers::vga::print_char(byte);
    });
}

/// Dump all buffered log messages to serial (for debugging).
#[allow(dead_code)]
pub fn dump_log_to_serial() {
    read_log(|byte| {
        crate::drivers::serial::write_byte(byte);
    });
}

// ============================================================================
// Rate Limiting
// ============================================================================

/// Rate limiter for preventing log flooding.
///
/// Use with `kprint_ratelimited!` macro or create static instances
/// with `define_ratelimit!`.
pub struct RateLimiter {
    last_print_ticks: AtomicU64,
    suppressed_count: AtomicU64,
    interval_ticks: u64,
}

impl RateLimiter {
    /// Create a new rate limiter with the specified interval in timer ticks.
    ///
    /// At ~100 Hz, 100 ticks = 1 second.
    pub const fn new(interval_ticks: u64) -> Self {
        Self {
            last_print_ticks: AtomicU64::new(0),
            suppressed_count: AtomicU64::new(0),
            interval_ticks,
        }
    }

    /// Check if printing is allowed.
    ///
    /// Returns `(allowed, suppressed_count)`. If allowed is true after
    /// suppression, suppressed_count indicates how many messages were
    /// suppressed since the last print.
    pub fn check(&self) -> (bool, u64) {
        let now = TIMER_TICKS.load(Ordering::Relaxed);
        let last = self.last_print_ticks.load(Ordering::Relaxed);

        if now.saturating_sub(last) >= self.interval_ticks {
            self.last_print_ticks.store(now, Ordering::Relaxed);
            let suppressed = self.suppressed_count.swap(0, Ordering::Relaxed);
            (true, suppressed)
        } else {
            self.suppressed_count.fetch_add(1, Ordering::Relaxed);
            (false, 0)
        }
    }

    /// Reset the rate limiter state.
    #[allow(dead_code)]
    pub fn reset(&self) {
        self.last_print_ticks.store(0, Ordering::Relaxed);
        self.suppressed_count.store(0, Ordering::Relaxed);
    }
}

// ============================================================================
// Macros
// ============================================================================

/// Core print macro (no level, no newline).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::log::_kprint(format_args!($($arg)*))
    };
}

/// Core print macro with newline.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {
        $crate::log::_kprint(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Emergency level (system unusable).
#[macro_export]
macro_rules! pr_emerg {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Emergency,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Alert level (immediate action required).
#[macro_export]
macro_rules! pr_alert {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Alert,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Critical conditions.
#[macro_export]
macro_rules! pr_crit {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Critical,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Error conditions.
#[macro_export]
macro_rules! pr_err {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Error,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Warning conditions.
#[macro_export]
macro_rules! pr_warn {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Warning,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Normal but significant condition.
#[macro_export]
macro_rules! pr_notice {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Notice,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Informational.
#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Info,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Debug messages (compiled out in release builds).
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        $crate::log::_kprint_with_level(
            $crate::log::LogLevel::Debug,
            format_args!("{}\n", format_args!($($arg)*))
        )
    };
}

/// Rate-limited print macro.
///
/// Usage:
/// ```ignore
/// define_ratelimit!(MY_LIMITER, 100);  // 100 ticks = ~1 second
/// kprint_ratelimited!(MY_LIMITER, "Repeated message: {}", value);
/// ```
#[macro_export]
macro_rules! kprint_ratelimited {
    ($limiter:expr, $($arg:tt)*) => {{
        let (allowed, suppressed) = $limiter.check();
        if allowed {
            if suppressed > 0 {
                $crate::kprintln!("[{} messages suppressed]", suppressed);
            }
            $crate::kprintln!($($arg)*);
        }
    }};
}

/// Define a static rate limiter.
///
/// Usage:
/// ```ignore
/// define_ratelimit!(MY_LIMITER, 100);  // 100 ticks interval
/// ```
#[macro_export]
macro_rules! define_ratelimit {
    ($name:ident, $interval_ticks:expr) => {
        static $name: $crate::log::RateLimiter = $crate::log::RateLimiter::new($interval_ticks);
    };
}
