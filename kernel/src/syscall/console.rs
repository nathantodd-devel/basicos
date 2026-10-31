//! The console as a file: fds 0/1/2 of every process start out here.
//!
//! The console is the VGA screen and PS/2 keyboard, mirrored on the serial
//! port: output and echo go to both, and input is taken from both, so
//! `-nographic` and the headless tests drive exactly what's on the screen.
//!
//! Input is a small cooked-mode line discipline: bytes are collected into
//! a line with echo and backspace, and `read` only returns once a whole
//! line (ending in `\n`) is in — the same contract a terminal in canonical
//! mode gives, which is what programs written against `read(0, ..)`
//! expect. Ctrl-D on an empty line is end-of-file; Ctrl-C discards the
//! line being typed. There's no line editing: the escape sequences that
//! arrows and function keys send are discarded.
//!
//! **A completed line is handed to exactly one reader, whole.** This is
//! the one console on the machine, and more than one task can be
//! blocked reading it at once — the kernel's own `shell` task and any
//! ring-3 process's `read(0, ..)` both end up here (see `shell`'s
//! module docs). A line used to sit in `ready` as flat bytes, taken one
//! `read()` at a time; both callers here read a line one byte per
//! syscall, so a preemption between two of those calls could hand the
//! next byte to whichever *other* task's turn it was — tearing one
//! line across two readers. The task that started it would then block
//! forever short one byte (usually the trailing `\n`), having watched
//! someone else quietly finish its line. `ready` now queues whole lines
//! (`Vec<u8>`, trailing `\n` included), and `take_ready` only ever
//! hands one out complete: torn *within* a line is gone; torn *between*
//! readers — which of several already-waiting readers gets the next
//! completed line — is inherent to sharing one input stream and stays
//! (matching ordinary Unix processes racing to read one pipe or tty).

use super::errno;
use crate::sync::Spinlock;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Longest line `take_ready` will hand out. Enforced on the way in
/// (`feed` stops growing `editing` here), so a completed line never
/// exceeds it — a reader's buffer shorter than this would otherwise
/// never see that line at all (`take_ready` won't tear one to fit).
/// Generous for an interactive command line; callers size their
/// buffers to match (`shell::LINE_BUF_SIZE`, the SDK's `io::MAX_LINE`).
pub const MAX_LINE: usize = 256;

struct Line {
    /// The line being typed (not yet visible to `read`).
    editing: Vec<u8>,
    /// Completed lines waiting to be read, oldest first — each whole
    /// (trailing `\n` included, except the EOF-with-pending-text case),
    /// never longer than `MAX_LINE`.
    ready: VecDeque<Vec<u8>>,
    /// Ctrl-D on an empty line: the next `read` returns 0 once.
    eof: bool,
    /// How far through an escape sequence the input is.
    esc: Escape,
}

/// An escape sequence on its way through `feed`.
#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    /// Saw ESC.
    Start,
    /// In `ESC [ ...`, up to a final byte.
    Csi,
    /// In `ESC O x`, one byte to go.
    Ss3,
}

static LINE: Spinlock<Line> = Spinlock::new(Line {
    editing: Vec::new(),
    ready: VecDeque::new(),
    eof: false,
    esc: Escape::None,
});

/// Whether any input driver is compiled in at all.
pub const HAS_INPUT: bool = cfg!(any(feature = "serial-input", feature = "keyboard"));

fn next_input_byte() -> Option<u8> {
    #[cfg(feature = "serial-input")]
    if let Some(b) = crate::drivers::serial::try_read_byte() {
        return Some(b);
    }
    #[cfg(feature = "keyboard")]
    if let Some(b) = crate::drivers::keyboard::try_read_byte() {
        return Some(b);
    }
    None
}

/// Show typing on both halves of the console. (Serial is a dumb pipe and
/// the keyboard leaves echo to us, so neither does it for itself.)
fn echo(bytes: &[u8]) {
    for &b in bytes {
        crate::drivers::vga::print_char(b);
        crate::drivers::serial::write_byte(b);
    }
}

/// Feed one raw input byte through the line discipline.
fn feed(line: &mut Line, byte: u8) {
    match line.esc {
        Escape::None => {}
        Escape::Start => {
            line.esc = match byte {
                b'[' => Escape::Csi,
                b'O' => Escape::Ss3,
                _ => Escape::None,
            };
            return;
        }
        Escape::Csi => {
            if (0x40..=0x7E).contains(&byte) {
                line.esc = Escape::None;
            }
            return;
        }
        Escape::Ss3 => {
            line.esc = Escape::None;
            return;
        }
    }
    match byte {
        0x1B => line.esc = Escape::Start,
        b'\r' | b'\n' => {
            echo(b"\r\n");
            let mut completed: Vec<u8> = line.editing.drain(..).collect();
            completed.push(b'\n');
            line.ready.push_back(completed);
        }
        0x7F | 0x08 => {
            if line.editing.pop().is_some() {
                echo(&[0x08, b' ', 0x08]);
            }
        }
        0x04 => {
            // Ctrl-D: with text pending it just completes the read (no
            // trailing '\n' — there wasn't one); on an empty line it's
            // end-of-file.
            if line.editing.is_empty() {
                line.eof = true;
            } else {
                let completed: Vec<u8> = line.editing.drain(..).collect();
                line.ready.push_back(completed);
            }
        }
        0x03 => {
            echo(b"^C\r\n");
            line.editing.clear();
        }
        // Room for one more plus the '\n' `\r`/`\n`/Ctrl-D above will
        // append — a line at exactly MAX_LINE would otherwise never fit
        // any reader's buffer and could never be delivered.
        c if (0x20..0x7F).contains(&c) => {
            if line.editing.len() + 1 < MAX_LINE {
                line.editing.push(c);
                echo(&[c]);
            }
            // Past the limit: dropped, same as a real terminal's
            // MAX_CANON refusing more input until the line is consumed.
        }
        _ => {}
    }
}

/// Take the next whole completed line into `out`, if one is ready and
/// fits (`out.len() >= that line's length`); if it doesn't fit, it's
/// left queued rather than torn — see this module's doc comment.
fn take_ready(out: &mut [u8]) -> Option<usize> {
    let mut line = LINE.lock();
    if let Some(front) = line.ready.front() {
        if front.len() <= out.len() {
            let l = line.ready.pop_front().expect("just checked front()");
            out[..l.len()].copy_from_slice(&l);
            return Some(l.len());
        }
        return None; // too small for the line at hand; try again
    }
    if line.eof {
        line.eof = false;
        return Some(0);
    }
    None
}

/// Whether a `read` would return immediately (data or EOF pending).
pub fn readable() -> bool {
    if !HAS_INPUT {
        return true; // reads return EOF straight away
    }
    pump();
    let line = LINE.lock();
    !line.ready.is_empty() || line.eof
}

/// Pull whatever the driver has into the line discipline.
fn pump() {
    while let Some(b) = next_input_byte() {
        feed(&mut LINE.lock(), b);
    }
}

pub fn read(out: &mut [u8], nonblock: bool) -> i64 {
    if out.is_empty() {
        return 0;
    }
    if !HAS_INPUT {
        return 0; // nothing to read from: EOF
    }
    loop {
        pump();
        if let Some(n) = take_ready(out) {
            return n as i64;
        }
        if nonblock {
            return errno::EAGAIN;
        }
        super::relax();
    }
}

pub fn write(bytes: &[u8]) -> i64 {
    if let Ok(s) = core::str::from_utf8(bytes) {
        crate::drivers::vga::print(s);
        crate::drivers::serial::write_str(s);
    } else {
        for &b in bytes {
            crate::drivers::vga::print_char(b);
            crate::drivers::serial::write_byte(b);
        }
    }
    bytes.len() as i64
}

/// Bytes queued across every completed line (`FIONREAD`) — the whole
/// input queue's size, same as a real tty reports, not necessarily what
/// the next single `read()` call would return (that's capped at one
/// line).
pub fn available() -> usize {
    pump();
    LINE.lock().ready.iter().map(Vec::len).sum()
}
