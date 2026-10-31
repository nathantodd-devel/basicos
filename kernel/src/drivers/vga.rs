//! VGA text-mode (0xb8000) console, 80x25, with scrollback.
//!
//! The text on screen lives in [`Console`], and the hardware buffer is a
//! view of it: normally the live screen, or, after Shift+PageUp (see
//! `scroll_view`), a window further back in the history. Any new output
//! jumps back to the live screen.
//!
//! Every public function runs with interrupts off, so that a process
//! writing to the console, a kernel log line and a keypress can't tear each
//! other's updates. There's deliberately no lock: a panic has to be able to
//! print whatever was happening when it fired.

use crate::arch::interrupts;
use crate::arch::port::outb;
use core::fmt::{self, Write};

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const CELLS: usize = WIDTH * HEIGHT;
const BLANK: u16 = 0x0700 | b' ' as u16;
const LIGHT_GREY_ON_BLACK: u16 = 0x0700;
/// Lines kept after they scroll off the top.
const HISTORY_LINES: usize = 500;
const TAB_STOP: usize = 8;

struct Console {
    row: usize,
    /// Reaches `WIDTH` after the last column is written, and wraps on the
    /// next glyph: a line of exactly 80 characters then a newline is one
    /// line, not a line and a blank.
    col: usize,
    screen: [u16; CELLS],
    /// A ring of the lines that have scrolled off, `history_len` of them
    /// ending just before `history_next`.
    history: [[u16; WIDTH]; HISTORY_LINES],
    history_next: usize,
    history_len: usize,
    /// How many lines back from the live screen the view is.
    view: usize,
}

static mut CONSOLE: Console = Console {
    row: 0,
    col: 0,
    screen: [BLANK; CELLS],
    history: [[BLANK; WIDTH]; HISTORY_LINES],
    history_next: 0,
    history_len: 0,
    view: 0,
};

/// Run `f` on the console with interrupts off, then move the hardware
/// cursor to match.
fn with_console<R>(f: impl FnOnce(&mut Console) -> R) -> R {
    let was_enabled = interrupts::disable();
    // Safety: single core and interrupts are off, so this is the only
    // reference to `CONSOLE` for as long as it lives.
    let console = unsafe { &mut *core::ptr::addr_of_mut!(CONSOLE) };
    let r = f(console);
    console.update_cursor();
    interrupts::restore(was_enabled);
    r
}

impl Console {
    /// Show the view on the hardware.
    fn repaint(&self) {
        for row in 0..HEIGHT {
            for col in 0..WIDTH {
                let cell = self.cell_in_view(row, col);
                unsafe { VGA_BUFFER.add(row * WIDTH + col).write_volatile(cell) };
            }
        }
    }

    fn cell_in_view(&self, row: usize, col: usize) -> u16 {
        if row >= self.view {
            return self.screen[(row - self.view) * WIDTH + col];
        }
        let back = self.view - row; // 1 = the newest line of history
        let line = (self.history_next + HISTORY_LINES - back) % HISTORY_LINES;
        self.history[line][col]
    }

    fn show_live(&mut self) {
        if self.view != 0 {
            self.view = 0;
            self.repaint();
        }
    }

    fn put_cell(&mut self, idx: usize, cell: u16) {
        self.screen[idx] = cell;
        unsafe { VGA_BUFFER.add(idx).write_volatile(cell) };
    }

    fn scroll(&mut self) {
        let line = &mut self.history[self.history_next];
        line.copy_from_slice(&self.screen[..WIDTH]);
        self.history_next = (self.history_next + 1) % HISTORY_LINES;
        self.history_len = (self.history_len + 1).min(HISTORY_LINES);

        self.screen.copy_within(WIDTH.., 0);
        self.screen[CELLS - WIDTH..].fill(BLANK);
        self.repaint();
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= HEIGHT {
            self.scroll();
            self.row = HEIGHT - 1;
        }
    }

    /// Print one CP437 glyph, or act on a control character.
    fn putc(&mut self, byte: u8) {
        self.show_live();
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            // Moves without erasing, as on a terminal: erasing is the
            // caller's "back, space, back".
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = WIDTH - 1;
                }
            }
            b'\t' => {
                for _ in 0..TAB_STOP - self.col % TAB_STOP {
                    self.putc(b' ');
                }
            }
            0..=0x1F | 0x7F => {} // no glyph wanted for the rest
            _ => {
                if self.col >= WIDTH {
                    self.newline();
                }
                self.put_cell(self.row * WIDTH + self.col, LIGHT_GREY_ON_BLACK | byte as u16);
                self.col += 1;
            }
        }
    }

    fn update_cursor(&self) {
        // While looking at history the cursor has nowhere to be, so park
        // it past the end of the screen, which hides it.
        let pos = if self.view == 0 { self.row * WIDTH + self.col.min(WIDTH - 1) } else { CELLS } as u16;
        unsafe {
            outb(0x3D4, 0x0F);
            outb(0x3D5, (pos & 0xFF) as u8);
            outb(0x3D4, 0x0E);
            outb(0x3D5, ((pos >> 8) & 0xFF) as u8);
        }
    }
}

/// The code page 437 glyph for `c`. VGA text mode has no Unicode, and text
/// reaching it is UTF-8.
fn to_cp437(c: char) -> u8 {
    match c {
        '\0'..='\x7F' => c as u8,
        '—' | '–' | '─' => 0xC4,
        '│' => 0xB3,
        '·' => 0xFA,
        '°' => 0xF8,
        '→' => 0x1A,
        '←' => 0x1B,
        '↑' => 0x18,
        '↓' => 0x19,
        _ => b'?',
    }
}

/// Writer for VGA text mode output implementing `core::fmt::Write`.
pub struct VgaWriter;

impl Write for VgaWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        print(s);
        Ok(())
    }
}

/// Global VGA writer instance.
#[allow(dead_code)]
pub static mut VGA_WRITER: VgaWriter = VgaWriter;

/// Blank the screen and forget the scrollback.
pub fn clear_screen() {
    with_console(|c| {
        c.screen.fill(BLANK);
        c.history_len = 0;
        c.view = 0;
        c.row = 0;
        c.col = 0;
        c.repaint();
    });
}

/// Move the view by `lines` through the scrollback: positive is back in
/// time, negative towards the live screen.
pub fn scroll_view(lines: isize) {
    with_console(|c| {
        let view = c.view.saturating_add_signed(lines).min(c.history_len);
        if view != c.view {
            c.view = view;
            c.repaint();
        }
    });
}

#[allow(dead_code)]
pub fn print_line(s: &str) {
    with_console(|con| {
        s.chars().for_each(|c| con.putc(to_cp437(c)));
        con.putc(b'\n');
    });
}

pub fn print(s: &str) {
    with_console(|con| s.chars().for_each(|c| con.putc(to_cp437(c))));
}

/// Print one raw byte: a CP437 glyph or a control character.
pub fn print_char(byte: u8) {
    with_console(|con| con.putc(byte));
}

/// Erase the previous character, wrapping back onto the line before if
/// that's where it is.
pub fn backspace() {
    print("\x08 \x08");
}
