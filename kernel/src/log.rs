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
//!
//! Messages less serious than [`MAX_LEVEL`] are compiled out, leaving no
//! code or strings in the kernel. Build with `make LOG_LEVEL=warn` to set
//! it. Unlike the runtime thresholds below, which only decide what's shown,
//! a stripped message doesn't reach the log buffer, so `dmesg` won't have it.
//!
//! A line built up in pieces starts with `pr_start!`, which adds no newline,
//! and goes on with `pr_cont!`, which adds no prefix or newline either:
//!
//! ```ignore
//! pr_start!(Info, "Probing:");
//! pr_cont!(" ata");
//! pr_cont!(" pci\n");
//! ```
//!
//! (`pr_info!` and friends end their own line, unlike Linux's, so the
//! opener has to be explicit.) Another task can log in the middle of that.
//! Its message doesn't join the line: the open line is ended first, and
//! a `pr_cont!` that finds its line gone starts a new one at the same level.

use crate::arch::interrupts::TIMER_TICKS;
use crate::drivers::serial::SerialWriter;
use crate::drivers::vga::VgaWriter;
use crate::sync::{LogRingBuffer, Spinlock};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

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
    fn from_u8(level: u8) -> LogLevel {
        match level {
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

/// The least serious level compiled in, from `BASICOS_LOG_LEVEL` (the
/// Makefile's `LOG_LEVEL`). By default that's everything in a debug build,
/// and all but `pr_debug!` in a release one.
pub const MAX_LEVEL: LogLevel = match option_env!("BASICOS_LOG_LEVEL") {
    Some(name) if !name.is_empty() => parse_level(name),
    _ if cfg!(debug_assertions) => LogLevel::Debug,
    _ => LogLevel::Info,
};

/// Whether messages at `level` are compiled in.
pub const fn enabled(level: LogLevel) -> bool {
    level as u8 <= MAX_LEVEL as u8
}

const fn parse_level(name: &str) -> LogLevel {
    match name.as_bytes() {
        b"emerg" => LogLevel::Emergency,
        b"alert" => LogLevel::Alert,
        b"crit" => LogLevel::Critical,
        b"err" => LogLevel::Error,
        b"warn" => LogLevel::Warning,
        b"notice" => LogLevel::Notice,
        b"info" => LogLevel::Info,
        b"debug" => LogLevel::Debug,
        _ => panic!("LOG_LEVEL must be one of emerg, alert, crit, err, warn, notice, info, debug"),
    }
}

// ============================================================================
// Global State
// ============================================================================

/// Ring buffer size: 16KB (must be power of 2)
pub const LOG_BUFFER_SIZE: usize = 16384;

/// Global log buffer protected by spinlock.
static LOG_BUFFER: Spinlock<LogRingBuffer<LOG_BUFFER_SIZE>> =
    Spinlock::new(LogRingBuffer::new());

/// Current console log level threshold.
/// Messages with level <= this value are displayed on console.
static CONSOLE_LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// Threshold for the VGA screen, on top of `CONSOLE_LOG_LEVEL`. Lowered
/// once boot is over, when the screen belongs to the shell.
static VGA_LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Debug as u8);

/// Enable/disable VGA output.
static VGA_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enable/disable serial output.
static SERIAL_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enable/disable timestamps.
static TIMESTAMPS_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the last message left its line unfinished, for `pr_cont!`. These
/// are only touched with interrupts off.
static LINE_OPEN: AtomicBool = AtomicBool::new(false);
/// The task that left it open: only that task can continue it.
static LINE_OWNER: AtomicUsize = AtomicUsize::new(0);
/// Set when this task's `pr_start!` was compiled out: its `pr_cont!`s can't
/// be stripped too, having no level of their own, so they're dropped here
/// until one ends the line.
static LINE_STRIPPED: AtomicBool = AtomicBool::new(false);
static STRIPPED_OWNER: AtomicUsize = AtomicUsize::new(0);
/// The level of the latest message, which a continuation inherits.
static LAST_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

// ============================================================================
// Console Writer
// ============================================================================

/// Internal writer that dispatches to all enabled output backends
/// and stores messages in the ring buffer.
struct ConsoleWriter {
    level: LogLevel,
    /// The last byte written, to tell whether the line was finished.
    last: u8,
}

impl Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if let Some(&byte) = s.as_bytes().last() {
            self.last = byte;
        }

        // Always write to ring buffer
        {
            let mut buf = LOG_BUFFER.lock();
            buf.write(s.as_bytes());
        }

        // Only output to console if level <= threshold
        let threshold = CONSOLE_LOG_LEVEL.load(Ordering::Relaxed);
        if (self.level as u8) <= threshold {
            let vga_threshold = VGA_LOG_LEVEL.load(Ordering::Relaxed);
            if VGA_ENABLED.load(Ordering::Relaxed) && (self.level as u8) <= vga_threshold {
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
    end_open_line();
    write_message(level, args);
    crate::arch::interrupts::restore(was_enabled);
}

/// Start a line that `pr_cont!` will go on with. Used by `pr_start!`.
#[doc(hidden)]
pub fn _kprint_start(level: LogLevel, args: fmt::Arguments) {
    let was_enabled = crate::arch::interrupts::disable();
    if STRIPPED_OWNER.load(Ordering::Relaxed) == current_task() {
        LINE_STRIPPED.store(false, Ordering::Relaxed);
    }
    end_open_line();
    write_message(level, args);
    crate::arch::interrupts::restore(was_enabled);
}

/// A `pr_start!` that was compiled out.
#[doc(hidden)]
pub fn _kprint_start_stripped() {
    let was_enabled = crate::arch::interrupts::disable();
    STRIPPED_OWNER.store(current_task(), Ordering::Relaxed);
    LINE_STRIPPED.store(true, Ordering::Relaxed);
    crate::arch::interrupts::restore(was_enabled);
}

/// Consume a `pr_cont!` whose line was compiled out. True if it was one.
fn drop_stripped(args: fmt::Arguments) -> bool {
    if !LINE_STRIPPED.load(Ordering::Relaxed)
        || STRIPPED_OWNER.load(Ordering::Relaxed) != current_task()
    {
        return false;
    }
    struct LastByte(u8);
    impl Write for LastByte {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0 = s.as_bytes().last().copied().unwrap_or(self.0);
            Ok(())
        }
    }
    let mut last = LastByte(0);
    let _ = last.write_fmt(args);
    if last.0 == b'\n' {
        LINE_STRIPPED.store(false, Ordering::Relaxed);
    }
    true
}

/// Go on with the line this task left open. Used by `pr_cont!`.
#[doc(hidden)]
pub fn _kprint_cont(args: fmt::Arguments) {
    let was_enabled = crate::arch::interrupts::disable();
    if drop_stripped(args) {
        crate::arch::interrupts::restore(was_enabled);
        return;
    }
    let level = LogLevel::from_u8(LAST_LEVEL.load(Ordering::Relaxed));
    let ours = LINE_OPEN.load(Ordering::Relaxed)
        && LINE_OWNER.load(Ordering::Relaxed) == current_task();
    if ours {
        let mut writer = ConsoleWriter { level, last: 0 };
        let _ = writer.write_fmt(args);
        note_line_end(&writer);
    } else {
        end_open_line();
        write_message(level, args);
    }
    crate::arch::interrupts::restore(was_enabled);
}

/// Write a message with its timestamp and level prefix. Interrupts must be
/// off and no line open.
fn write_message(level: LogLevel, args: fmt::Arguments) {
    LAST_LEVEL.store(level as u8, Ordering::Relaxed);
    let mut writer = ConsoleWriter { level, last: 0 };

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
    note_line_end(&writer);
}

/// Record whether `writer` left its line unfinished.
fn note_line_end(writer: &ConsoleWriter) {
    let open = writer.last != b'\n';
    LINE_OPEN.store(open, Ordering::Relaxed);
    if open {
        LINE_OWNER.store(current_task(), Ordering::Relaxed);
    }
}

/// Finish an unfinished line, so what comes next can't join it. It's written
/// at the line's own level, or the outputs that showed it would never get
/// the newline.
fn end_open_line() {
    if LINE_OPEN.swap(false, Ordering::Relaxed) {
        let level = LogLevel::from_u8(LAST_LEVEL.load(Ordering::Relaxed));
        let _ = ConsoleWriter { level, last: 0 }.write_str("\n");
    }
}

fn current_task() -> usize {
    #[cfg(feature = "scheduler")]
    return crate::sched::current_index();
    #[cfg(not(feature = "scheduler"))]
    return 0;
}

/// Print without level prefix (raw output).
/// Used by kprint!/kprintln!
///
/// Same interrupts-disabled-for-the-whole-call treatment as
/// `_kprint_with_level`, for the same reason.
#[doc(hidden)]
pub fn _kprint(args: fmt::Arguments) {
    let was_enabled = crate::arch::interrupts::disable();
    end_open_line();
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
    LogLevel::from_u8(CONSOLE_LOG_LEVEL.load(Ordering::Relaxed))
}

/// Show only messages at `level` or more serious on the VGA screen. Serial
/// and the log buffer aren't affected.
pub fn set_vga_log_level(level: LogLevel) {
    VGA_LOG_LEVEL.store(level as u8, Ordering::Relaxed);
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

/// Whether the buffer has overwritten its oldest messages, so that what
/// `read_log` returns may start part-way through a line.
pub fn log_wrapped() -> bool {
    LOG_BUFFER.lock().total_written() > LOG_BUFFER_SIZE
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

/// The body of the `pr_*!` macros: a whole line at `$level`, if that's
/// compiled in.
#[doc(hidden)]
#[macro_export]
macro_rules! __pr {
    ($level:ident, $($arg:tt)*) => {
        if const { $crate::log::enabled($crate::log::LogLevel::$level) } {
            $crate::log::_kprint_with_level(
                $crate::log::LogLevel::$level,
                format_args!("{}\n", format_args!($($arg)*))
            )
        }
    };
}

/// Emergency level (system unusable).
#[macro_export]
macro_rules! pr_emerg {
    ($($arg:tt)*) => {
        $crate::__pr!(Emergency, $($arg)*)
    };
}

/// Alert level (immediate action required).
#[macro_export]
macro_rules! pr_alert {
    ($($arg:tt)*) => {
        $crate::__pr!(Alert, $($arg)*)
    };
}

/// Critical conditions.
#[macro_export]
macro_rules! pr_crit {
    ($($arg:tt)*) => {
        $crate::__pr!(Critical, $($arg)*)
    };
}

/// Error conditions.
#[macro_export]
macro_rules! pr_err {
    ($($arg:tt)*) => {
        $crate::__pr!(Error, $($arg)*)
    };
}

/// Warning conditions.
#[macro_export]
macro_rules! pr_warn {
    ($($arg:tt)*) => {
        $crate::__pr!(Warning, $($arg)*)
    };
}

/// Normal but significant condition.
#[macro_export]
macro_rules! pr_notice {
    ($($arg:tt)*) => {
        $crate::__pr!(Notice, $($arg)*)
    };
}

/// Informational.
#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {
        $crate::__pr!(Info, $($arg)*)
    };
}

/// Start a line for `pr_cont!` to go on with, at the given level:
/// `pr_start!(Info, "Probing:")`. No newline is added.
#[macro_export]
macro_rules! pr_start {
    ($level:ident, $($arg:tt)*) => {
        if const { $crate::log::enabled($crate::log::LogLevel::$level) } {
            $crate::log::_kprint_start($crate::log::LogLevel::$level, format_args!($($arg)*))
        } else {
            $crate::log::_kprint_start_stripped()
        }
    };
}

/// Go on with the line `pr_start!` began. No prefix or newline is added: end
/// the line by including a `\n`.
#[macro_export]
macro_rules! pr_cont {
    ($($arg:tt)*) => {
        $crate::log::_kprint_cont(format_args!($($arg)*))
    };
}

/// Debug messages (compiled out of release builds by default).
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => {
        $crate::__pr!(Debug, $($arg)*)
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
