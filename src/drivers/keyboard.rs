//! PS/2 keyboard driver.
//!
//! The PS/2 controller (behind port 0x60) uses "scancode set 1" by
//! default on real hardware and in QEMU. Each key has a "make code"
//! sent on press; releasing it sends the same code with the top bit
//! set (make | 0x80), except for a couple of two-byte extended codes
//! (0xE0-prefixed) that we don't handle yet — that's what real arrow
//! keys, right-Ctrl, etc. use.

use super::vga;
use core::sync::atomic::{AtomicBool, Ordering};

const LEFT_SHIFT_PRESS: u8 = 0x2A;
const RIGHT_SHIFT_PRESS: u8 = 0x36;
const LEFT_SHIFT_RELEASE: u8 = 0xAA;
const RIGHT_SHIFT_RELEASE: u8 = 0xB6;
const RELEASE_BIT: u8 = 0x80;

const BACKSPACE: u8 = 0x0E;
const ENTER: u8 = 0x1C;

static SHIFT_HELD: AtomicBool = AtomicBool::new(false);

/// Called from the IRQ1 handler with the raw byte read from port 0x60.
pub fn on_scancode(code: u8) {
    match code {
        LEFT_SHIFT_PRESS | RIGHT_SHIFT_PRESS => {
            SHIFT_HELD.store(true, Ordering::Relaxed);
            return;
        }
        LEFT_SHIFT_RELEASE | RIGHT_SHIFT_RELEASE => {
            SHIFT_HELD.store(false, Ordering::Relaxed);
            return;
        }
        _ => {}
    }

    // any other release (top bit set) — ignore for now, we're not
    // tracking held-key repeat/state beyond shift yet
    if code & RELEASE_BIT != 0 {
        return;
    }

    if code == BACKSPACE {
        vga::backspace();
        return;
    }

    if code == ENTER {
        vga::print_char(b'\n');
        return;
    }

    let shift = SHIFT_HELD.load(Ordering::Relaxed);
    if let Some(ascii) = to_ascii(code, shift) {
        vga::print_char(ascii);
    }
}

/// US QWERTY scancode-set-1 -> ASCII, for the make codes we handle.
/// Returns None for keys we don't map yet (function keys, arrows, etc.)
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
