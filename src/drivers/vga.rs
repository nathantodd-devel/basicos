//! VGA text-mode (0xb8000) driver, 80x25 with scrolling.

use crate::arch::port::outb;
use core::fmt::{self, Write};

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const LIGHT_GREY_ON_BLACK: u16 = 0x0700;

static mut ROW: usize = 0;
static mut COL: usize = 0;

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

pub fn clear_screen() {
    for i in 0..(WIDTH * HEIGHT) {
        unsafe {
            VGA_BUFFER
                .add(i)
                .write_volatile(LIGHT_GREY_ON_BLACK | b' ' as u16);
        }
    }
    unsafe {
        ROW = 0;
        COL = 0;
    }
}

#[allow(dead_code)]
pub fn print_line(s: &str) {
    print(s);
    newline();
}

pub fn print(s: &str) {
    for byte in s.bytes() {
        putc(byte);
    }
}

/// Print one byte (used by the keyboard driver for live typing).
pub fn print_char(byte: u8) {
    putc(byte);
}

/// Erase the previous character, if there is one on the current line.
/// Doesn't walk back onto the previous line yet.
pub fn backspace() {
    unsafe {
        if COL > 0 {
            COL -= 1;
            let idx = ROW * WIDTH + COL;
            VGA_BUFFER
                .add(idx)
                .write_volatile(LIGHT_GREY_ON_BLACK | b' ' as u16);
        }
    }
}

fn putc(byte: u8) {
    if byte == b'\n' {
        newline();
        update_cursor();
        return;
    }
    unsafe {
        let idx = ROW * WIDTH + COL;
        VGA_BUFFER
            .add(idx)
            .write_volatile(LIGHT_GREY_ON_BLACK | byte as u16);
        COL += 1;
        if COL >= WIDTH {
            newline();
        }
        update_cursor();
    }
}

fn scroll() {
    unsafe {
        for row in 1..HEIGHT {
            for col in 0..WIDTH {
                let src_idx = row * WIDTH + col;
                let dst_idx = (row - 1) * WIDTH + col;
                let value = VGA_BUFFER.add(src_idx).read_volatile();
                VGA_BUFFER.add(dst_idx).write_volatile(value);
            }
        }
        // Clear the last line
        for col in 0..WIDTH {
            let idx = (HEIGHT - 1) * WIDTH + col;
            VGA_BUFFER
                .add(idx)
                .write_volatile(LIGHT_GREY_ON_BLACK | b' ' as u16);
        }
    }
}

fn newline() {
    unsafe {
        COL = 0;
        ROW += 1;
        if ROW >= HEIGHT {
            scroll();
            ROW = HEIGHT - 1;
        }
    }
}

fn set_cursor(row: usize, col: usize) {
    let pos = (row * WIDTH + col) as u16;
    unsafe {
        outb(0x3D4, 0x0F);
        outb(0x3D5, (pos & 0xFF) as u8);
        outb(0x3D4, 0x0E);
        outb(0x3D5, ((pos >> 8) & 0xFF) as u8);
    }
}

fn update_cursor() {
    unsafe {
        set_cursor(ROW, COL);
    }
}
