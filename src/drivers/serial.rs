//! COM1 serial port driver for debug output and input.
//!
//! Provides serial I/O at 115200 baud, useful for debugging via
//! `qemu-system-x86_64 ... -serial stdio` or real hardware.

use crate::arch::port::{inb, outb};
use crate::sync::RingBuffer;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

// COM1 port addresses
const COM1_BASE: u16 = 0x3F8;
const COM1_DATA: u16 = COM1_BASE; // Data register (R/W)
const COM1_IER: u16 = COM1_BASE + 1; // Interrupt Enable Register
const COM1_FCR: u16 = COM1_BASE + 2; // FIFO Control Register
const COM1_LCR: u16 = COM1_BASE + 3; // Line Control Register
const COM1_MCR: u16 = COM1_BASE + 4; // Modem Control Register
const COM1_LSR: u16 = COM1_BASE + 5; // Line Status Register
#[allow(dead_code)]
const COM1_MSR: u16 = COM1_BASE + 6; // Modem Status Register
const COM1_DLL: u16 = COM1_BASE; // Divisor Latch Low (when DLAB=1)
const COM1_DLH: u16 = COM1_BASE + 1; // Divisor Latch High (when DLAB=1)

// IER bits
const IER_RX_AVAILABLE: u8 = 0x01; // Enable Received Data Available interrupt
#[allow(dead_code)]
const IER_TX_EMPTY: u8 = 0x02; // Enable Transmitter Holding Register Empty interrupt

// LSR bits
const LSR_DATA_READY: u8 = 0x01; // Data ready to read
const LSR_TX_EMPTY: u8 = 0x20; // Transmit buffer empty

/// Size of the receive buffer in bytes.
const RX_BUFFER_SIZE: usize = 256;

/// Receive buffer for incoming serial data.
static RX_BUFFER: RingBuffer<u8, RX_BUFFER_SIZE> = RingBuffer::new();

/// Flag indicating data is available in the receive buffer.
static DATA_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Initialize COM1 serial port at 115200 baud, 8N1 with receive interrupts.
///
/// # Safety
/// Must only be called once during kernel initialization.
pub unsafe fn init() {
    // Disable all interrupts while configuring
    outb(COM1_IER, 0x00);

    // Enable DLAB (Divisor Latch Access Bit) to set baud rate
    outb(COM1_LCR, 0x80);

    // Set divisor to 1 (115200 baud)
    // Divisor = 115200 / baud_rate, so 115200 / 115200 = 1
    outb(COM1_DLL, 0x01);
    outb(COM1_DLH, 0x00);

    // 8 bits, no parity, 1 stop bit (8N1), clear DLAB
    outb(COM1_LCR, 0x03);

    // Enable FIFO, clear TX/RX queues, 14-byte threshold
    outb(COM1_FCR, 0xC7);

    // Enable DTR, RTS, and OUT2 (OUT2 is required for interrupts on PC)
    outb(COM1_MCR, 0x0B);

    // Enable receive data available interrupt
    outb(COM1_IER, IER_RX_AVAILABLE);
}

/// Check if the transmit buffer is empty.
#[inline]
fn is_transmit_empty() -> bool {
    unsafe { inb(COM1_LSR) & LSR_TX_EMPTY != 0 }
}

/// Check if data is ready to be read.
#[inline]
fn is_data_ready() -> bool {
    unsafe { inb(COM1_LSR) & LSR_DATA_READY != 0 }
}

/// Write a single byte to COM1, blocking until the transmit buffer is ready.
pub fn write_byte(byte: u8) {
    // Wait for transmit buffer to be empty
    while !is_transmit_empty() {
        core::hint::spin_loop();
    }
    unsafe {
        outb(COM1_DATA, byte);
    }
}

/// Write a string to COM1.
pub fn write_str(s: &str) {
    for byte in s.bytes() {
        // Convert LF to CRLF for terminal compatibility
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

// --- Input functions ---

/// Called by the serial IRQ handler when data is received.
/// Reads all available bytes into the receive buffer.
pub fn on_interrupt() {
    while is_data_ready() {
        let byte = unsafe { inb(COM1_DATA) };
        // Try to push to buffer, dropping if full
        let _ = RX_BUFFER.try_push(byte);
        DATA_AVAILABLE.store(true, Ordering::Release);
    }
}

/// Read a byte from the receive buffer if available.
/// Returns `None` if the buffer is empty.
pub fn try_read_byte() -> Option<u8> {
    let byte = RX_BUFFER.try_pop();
    if RX_BUFFER.is_empty() {
        DATA_AVAILABLE.store(false, Ordering::Release);
    }
    byte
}

/// Read a byte from the receive buffer, blocking until data is available.
pub fn read_byte() -> u8 {
    loop {
        if let Some(byte) = try_read_byte() {
            return byte;
        }
        // Wait for data - in a real OS we'd yield to the scheduler
        core::hint::spin_loop();
    }
}

/// Check if there is data available to read.
pub fn has_data() -> bool {
    DATA_AVAILABLE.load(Ordering::Acquire)
}

/// Read a line (up to newline or buffer capacity) into the provided buffer.
/// Returns the number of bytes read (not including the newline).
/// Echoes characters back to the serial port.
pub fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let byte = read_byte();
        match byte {
            b'\r' | b'\n' => {
                write_byte(b'\r');
                write_byte(b'\n');
                break;
            }
            // Backspace handling
            0x7F | 0x08 => {
                if i > 0 {
                    i -= 1;
                    // Erase character on terminal
                    write_byte(0x08);
                    write_byte(b' ');
                    write_byte(0x08);
                }
            }
            // Ctrl+C
            0x03 => {
                write_byte(b'^');
                write_byte(b'C');
                write_byte(b'\r');
                write_byte(b'\n');
                return 0;
            }
            // Regular character
            c if c >= 0x20 && c < 0x7F => {
                buf[i] = c;
                i += 1;
                write_byte(c); // Echo
            }
            _ => {} // Ignore other control characters
        }
    }
    i
}

/// Read bytes directly from the serial port without using the buffer.
/// Useful for polling mode when interrupts are disabled.
pub fn poll_read_byte() -> Option<u8> {
    if is_data_ready() {
        Some(unsafe { inb(COM1_DATA) })
    } else {
        None
    }
}

// --- Output writer ---

/// Writer for serial output implementing `core::fmt::Write`.
pub struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Global serial writer instance.
#[allow(dead_code)]
pub static mut SERIAL_WRITER: SerialWriter = SerialWriter;
