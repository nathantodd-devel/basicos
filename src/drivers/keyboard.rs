//! PS/2 keyboard driver.
//!
//! Scancode set 1 (the QEMU/BIOS default): a "make code" is sent on
//! press, the same byte with the top bit set (make | 0x80) on release,
//! and two-byte 0xE0-prefixed codes cover the keys the original XT
//! keyboard didn't have — arrow keys, right Ctrl/Alt, Insert/Delete/
//! Home/End/PageUp/PageDown, and the keypad's Enter and `/`.
//!
//! Decoded output goes to two places:
//! - straight to the VGA console, for printable characters/backspace/
//!   enter/tab — local echo, so typing is visible immediately;
//! - into a small ring buffer other kernel code can read from with
//!   `read_byte`/`try_read_byte`/`read_line`, the same shape as
//!   `drivers::serial`'s input API.
//!
//! Non-printable keys (arrows, function keys, Home/End/...) only go
//! into the ring buffer, as the same ANSI/VT220 escape sequences a real
//! terminal would send (`ESC [ A` for Up, `ESC O P` for F1, ...) — there's
//! no separate "key event" type here, just one byte stream, like a tty.
//! They're *not* echoed to VGA: our text-mode console has no escape
//! interpreter, so printing them raw would just look like garbage.

use super::vga;
use crate::sync::RingBuffer;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

// --- scancodes (set 1) ------------------------------------------------

const EXTENDED_PREFIX: u8 = 0xE0;
const RELEASE_BIT: u8 = 0x80;

const LEFT_SHIFT: u8 = 0x2A;
const RIGHT_SHIFT: u8 = 0x36;
const LEFT_SHIFT_RELEASE: u8 = LEFT_SHIFT | RELEASE_BIT;
const RIGHT_SHIFT_RELEASE: u8 = RIGHT_SHIFT | RELEASE_BIT;

// Left Ctrl/Alt arrive unprefixed; Right Ctrl/Alt are the *same* base
// byte but 0xE0-prefixed (see `handle_extended`) — the keyboard
// controller has no other way to tell them apart.
const LEFT_CTRL: u8 = 0x1D;
const LEFT_CTRL_RELEASE: u8 = LEFT_CTRL | RELEASE_BIT;
const LEFT_ALT: u8 = 0x38;
const LEFT_ALT_RELEASE: u8 = LEFT_ALT | RELEASE_BIT;
const RIGHT_CTRL: u8 = 0x1D;
const RIGHT_CTRL_RELEASE: u8 = RIGHT_CTRL | RELEASE_BIT;
const RIGHT_ALT: u8 = 0x38;
const RIGHT_ALT_RELEASE: u8 = RIGHT_ALT | RELEASE_BIT;

const BACKSPACE: u8 = 0x0E;
const ENTER: u8 = 0x1C;
const KEYPAD_ENTER: u8 = 0x1C; // extended

// --- modifier state -----------------------------------------------------

/// Held modifier keys, as a small bitset. Left and right variants of a
/// key are folded into the same bit — nothing here needs to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers(u8);

impl Modifiers {
    const SHIFT: u8 = 1 << 0;
    const CTRL: u8 = 1 << 1;
    const ALT: u8 = 1 << 2;

    pub fn shift(self) -> bool {
        self.0 & Self::SHIFT != 0
    }
    pub fn ctrl(self) -> bool {
        self.0 & Self::CTRL != 0
    }
    pub fn alt(self) -> bool {
        self.0 & Self::ALT != 0
    }
}

static MODIFIER_BITS: AtomicU8 = AtomicU8::new(0);

/// Currently-held modifier keys.
pub fn modifiers() -> Modifiers {
    Modifiers(MODIFIER_BITS.load(Ordering::Relaxed))
}

fn set_modifier(bit: u8, held: bool) {
    if held {
        MODIFIER_BITS.fetch_or(bit, Ordering::Relaxed);
    } else {
        MODIFIER_BITS.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// Set when we've just seen the 0xE0 prefix byte — the *next* byte is
/// the real (extended) scancode, not a normal one.
static EXTENDED: AtomicBool = AtomicBool::new(false);

// --- input buffer --------------------------------------------------------

/// Must be a power of 2 (see `sync::RingBuffer`).
const BUFFER_SIZE: usize = 128;

static RX_BUFFER: RingBuffer<u8, BUFFER_SIZE> = RingBuffer::new();
static DATA_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Queue `bytes` for readers, optionally echoing them to VGA too.
/// Silently drops bytes if the queue is full — a slow/absent reader
/// shouldn't be able to wedge the keyboard IRQ.
fn emit(bytes: &[u8], echo_to_vga: bool) {
    if bytes.is_empty() {
        return;
    }
    for &b in bytes {
        let _ = RX_BUFFER.try_push(b);
    }
    DATA_AVAILABLE.store(true, Ordering::Release);
    if echo_to_vga {
        for &b in bytes {
            vga::print_char(b);
        }
    }
}

/// Called from the IRQ1 handler with the raw byte read from port 0x60.
pub fn on_scancode(code: u8) {
    if code == EXTENDED_PREFIX {
        EXTENDED.store(true, Ordering::Relaxed);
        return;
    }

    if EXTENDED.swap(false, Ordering::Relaxed) {
        handle_extended(code);
    } else {
        handle_normal(code);
    }
}

fn handle_normal(code: u8) {
    match code {
        LEFT_SHIFT | RIGHT_SHIFT => return set_modifier(Modifiers::SHIFT, true),
        LEFT_SHIFT_RELEASE | RIGHT_SHIFT_RELEASE => return set_modifier(Modifiers::SHIFT, false),
        LEFT_CTRL => return set_modifier(Modifiers::CTRL, true),
        LEFT_CTRL_RELEASE => return set_modifier(Modifiers::CTRL, false),
        LEFT_ALT => return set_modifier(Modifiers::ALT, true),
        LEFT_ALT_RELEASE => return set_modifier(Modifiers::ALT, false),
        _ => {}
    }

    // Any other release (top bit set) — ignore. We only track held
    // state for modifiers; normal keys are handled entirely on press.
    if code & RELEASE_BIT != 0 {
        return;
    }

    if code == BACKSPACE {
        vga::backspace(); // erase on screen ourselves...
        emit(&[0x08], false); // ...and queue it without re-printing
        return;
    }

    if code == ENTER {
        emit(b"\n", true);
        return;
    }

    if let Some(seq) = function_key_escape(code) {
        emit(seq, false);
        return;
    }

    let mods = modifiers();

    // Ctrl+letter produces the classic terminal control codes
    // (Ctrl+C = 0x03, Ctrl+D = 0x04, ...) instead of the plain letter —
    // `drivers::serial::read_line` already treats 0x03 as "Ctrl+C".
    if mods.ctrl() {
        if let Some(ascii) = to_ascii(code, false) {
            if let Some(ctrl_byte) = to_control(ascii) {
                emit(&[ctrl_byte], false);
                return;
            }
        }
    }

    if let Some(ascii) = to_ascii(code, mods.shift()) {
        emit(&[ascii], true);
    }
}

fn handle_extended(code: u8) {
    match code {
        RIGHT_CTRL => return set_modifier(Modifiers::CTRL, true),
        RIGHT_CTRL_RELEASE => return set_modifier(Modifiers::CTRL, false),
        RIGHT_ALT => return set_modifier(Modifiers::ALT, true),
        RIGHT_ALT_RELEASE => return set_modifier(Modifiers::ALT, false),
        _ => {}
    }

    if code & RELEASE_BIT != 0 {
        return;
    }

    if code == KEYPAD_ENTER {
        emit(b"\n", true);
        return;
    }

    if let Some(seq) = extended_key_escape(code) {
        emit(seq, false);
    }
}

/// Convert an already-shifted ASCII letter/symbol to its terminal
/// control-code equivalent (Ctrl+A -> 0x01, ..., Ctrl+Z -> 0x1A). Only
/// letters are mapped — digits/punctuation control combos exist too,
/// but letters cover every shortcut anything here actually needs.
fn to_control(byte: u8) -> Option<u8> {
    byte.is_ascii_alphabetic().then(|| byte.to_ascii_uppercase() & 0x1F)
}

/// F1-F12's escape sequences (VT100/xterm-style: SS3 for F1-F4, the
/// usual `CSI n ~` for F5-F12 — note the traditional gap at 16 and 22).
fn function_key_escape(code: u8) -> Option<&'static [u8]> {
    Some(match code {
        0x3B => b"\x1bOP",   // F1
        0x3C => b"\x1bOQ",   // F2
        0x3D => b"\x1bOR",   // F3
        0x3E => b"\x1bOS",   // F4
        0x3F => b"\x1b[15~", // F5
        0x40 => b"\x1b[17~", // F6
        0x41 => b"\x1b[18~", // F7
        0x42 => b"\x1b[19~", // F8
        0x43 => b"\x1b[20~", // F9
        0x44 => b"\x1b[21~", // F10
        0x57 => b"\x1b[23~", // F11
        0x58 => b"\x1b[24~", // F12
        _ => return None,
    })
}

/// Escape sequences for the 0xE0-prefixed keys that aren't modifiers or
/// keypad Enter: arrows (plain `CSI` letter) and the VT220 `CSI n ~` set.
fn extended_key_escape(code: u8) -> Option<&'static [u8]> {
    Some(match code {
        0x48 => b"\x1b[A",  // Up
        0x50 => b"\x1b[B",  // Down
        0x4D => b"\x1b[C",  // Right
        0x4B => b"\x1b[D",  // Left
        0x47 => b"\x1b[1~", // Home
        0x52 => b"\x1b[2~", // Insert
        0x53 => b"\x1b[3~", // Delete
        0x4F => b"\x1b[4~", // End
        0x49 => b"\x1b[5~", // PageUp
        0x51 => b"\x1b[6~", // PageDown
        _ => return None,
    })
}

/// US QWERTY scancode-set-1 -> ASCII, for the make codes we handle.
/// Returns None for keys we don't map (modifiers, function keys, etc.
/// — those are handled elsewhere).
fn to_ascii(code: u8, shift: bool) -> Option<u8> {
    let c = match code {
        0x02 => if shift { b'!' } else { b'1' },
        0x03 => if shift { b'@' } else { b'2' },
        0x04 => if shift { b'#' } else { b'3' },
        0x05 => if shift { b'$' } else { b'4' },
        0x06 => if shift { b'%' } else { b'5' },
        0x07 => if shift { b'^' } else { b'6' },
        0x08 => if shift { b'&' } else { b'7' },
        0x09 => if shift { b'*' } else { b'8' },
        0x0A => if shift { b'(' } else { b'9' },
        0x0B => if shift { b')' } else { b'0' },
        0x0C => if shift { b'_' } else { b'-' },
        0x0D => if shift { b'+' } else { b'=' },
        0x0F => b'\t',
        0x10 => if shift { b'Q' } else { b'q' },
        0x11 => if shift { b'W' } else { b'w' },
        0x12 => if shift { b'E' } else { b'e' },
        0x13 => if shift { b'R' } else { b'r' },
        0x14 => if shift { b'T' } else { b't' },
        0x15 => if shift { b'Y' } else { b'y' },
        0x16 => if shift { b'U' } else { b'u' },
        0x17 => if shift { b'I' } else { b'i' },
        0x18 => if shift { b'O' } else { b'o' },
        0x19 => if shift { b'P' } else { b'p' },
        0x1A => if shift { b'{' } else { b'[' },
        0x1B => if shift { b'}' } else { b']' },
        0x1E => if shift { b'A' } else { b'a' },
        0x1F => if shift { b'S' } else { b's' },
        0x20 => if shift { b'D' } else { b'd' },
        0x21 => if shift { b'F' } else { b'f' },
        0x22 => if shift { b'G' } else { b'g' },
        0x23 => if shift { b'H' } else { b'h' },
        0x24 => if shift { b'J' } else { b'j' },
        0x25 => if shift { b'K' } else { b'k' },
        0x26 => if shift { b'L' } else { b'l' },
        0x27 => if shift { b':' } else { b';' },
        0x28 => if shift { b'"' } else { b'\'' },
        0x29 => if shift { b'~' } else { b'`' },
        0x2B => if shift { b'|' } else { b'\\' },
        0x2C => if shift { b'Z' } else { b'z' },
        0x2D => if shift { b'X' } else { b'x' },
        0x2E => if shift { b'C' } else { b'c' },
        0x2F => if shift { b'V' } else { b'v' },
        0x30 => if shift { b'B' } else { b'b' },
        0x31 => if shift { b'N' } else { b'n' },
        0x32 => if shift { b'M' } else { b'm' },
        0x33 => if shift { b'<' } else { b',' },
        0x34 => if shift { b'>' } else { b'.' },
        0x35 => if shift { b'?' } else { b'/' },
        0x39 => b' ',
        _ => return None,
    };
    Some(c)
}

// --- reader API (mirrors drivers::serial's input side) -----------------

/// Read a byte from the queue if available. Returns `None` if empty.
pub fn try_read_byte() -> Option<u8> {
    let byte = RX_BUFFER.try_pop();
    if RX_BUFFER.is_empty() {
        DATA_AVAILABLE.store(false, Ordering::Release);
    }
    byte
}

/// Read a byte from the queue, blocking (spinning) until one arrives.
pub fn read_byte() -> u8 {
    loop {
        if let Some(byte) = try_read_byte() {
            return byte;
        }
        core::hint::spin_loop();
    }
}

/// Check whether there's queued input waiting to be read.
pub fn has_data() -> bool {
    DATA_AVAILABLE.load(Ordering::Acquire)
}

/// Read a line (up to newline or `buf`'s capacity) into `buf`. Returns
/// the number of bytes read (not including the newline), or 0 on
/// Ctrl+C. Unlike `drivers::serial::read_line`, this doesn't echo —
/// `on_scancode` already echoed each character live as it was typed.
pub fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        match read_byte() {
            b'\r' | b'\n' => break,
            0x7F | 0x08 => {
                if i > 0 {
                    i -= 1;
                }
            }
            0x03 => return 0, // Ctrl+C
            c if (0x20..0x7F).contains(&c) => {
                buf[i] = c;
                i += 1;
            }
            _ => {}
        }
    }
    i
}
