//! Panic handler: a full-screen VGA message (white on blue) plus an
//! `rbp`-chain stack trace, mirrored byte-for-byte to serial as it's
//! written.
//!
//! The serial mirror matters more than it looks: `make run`/
//! `run-release` and most of this project's own testing are headless
//! (`-serial stdio`/`-display none`) — a VGA-only panic would be
//! invisible to exactly that workflow. `drivers::serial::write_byte` is
//! raw, lock-free port I/O (just polls the UART's "ready" bit), so it's
//! safe to call here with interrupts off and everything else in the
//! kernel possibly mid-something.
//!
//! `cli` runs before any writing and nothing after it re-enables
//! interrupts — deliberately: `log.rs`'s `pr_*!` macros only disable
//! interrupts per call, not across a whole multi-line message, so with
//! the scheduler running, a panic inside a task could otherwise get
//! preempted mid-message and never reach the final `loop { hlt }`.
//!
//! The stack trace needs frame pointers preserved to walk `rbp`
//! (`-C force-frame-pointers=yes`, set in `.cargo/config.toml`) —
//! without that this prints nothing useful, silently.

use core::fmt::{self, Write};
use core::panic::PanicInfo;

const VGA_BUFFER: *mut u8 = 0xb8000 as *mut u8;
const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;
const PANIC_COLOR: u8 = 0x1F; // white on blue

struct PanicWriter {
    col: usize,
    row: usize,
}

impl PanicWriter {
    fn clear_screen(&mut self) {
        for row in 0..VGA_HEIGHT {
            for col in 0..VGA_WIDTH {
                let offset = 2 * (row * VGA_WIDTH + col);
                unsafe {
                    core::ptr::write_volatile(VGA_BUFFER.add(offset), b' ');
                    core::ptr::write_volatile(VGA_BUFFER.add(offset + 1), PANIC_COLOR);
                }
            }
        }
        self.col = 0;
        self.row = 0;
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= VGA_HEIGHT {
            self.row = VGA_HEIGHT - 1;
        }
    }

    fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.newline();
                // CRLF for serial — a real terminal needs the carriage
                // return too, same translation `drivers::serial::write_str`
                // normally does (we're bypassing it to write one byte at
                // a time, so it has to happen here instead).
                crate::drivers::serial::write_byte(b'\r');
                crate::drivers::serial::write_byte(b'\n');
            }
            byte => {
                if self.col >= VGA_WIDTH {
                    // VGA-only: wrap the 80-column display. Not mirrored
                    // to serial — this is a display artifact, not an
                    // actual line break in the message, and serial's own
                    // terminal wraps at whatever width it has anyway.
                    self.newline();
                }
                let offset = 2 * (self.row * VGA_WIDTH + self.col);
                unsafe {
                    core::ptr::write_volatile(VGA_BUFFER.add(offset), byte);
                    core::ptr::write_volatile(VGA_BUFFER.add(offset + 1), PANIC_COLOR);
                }
                self.col += 1;
                crate::drivers::serial::write_byte(byte);
            }
        }
    }
}

impl Write for PanicWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' => self.write_byte(byte),
                _ => self.write_byte(0xfe),
            }
        }
        Ok(())
    }
}

/// Walk the `rbp` chain to print return addresses.
///
/// Requires frame pointers to be preserved: build with
/// `-C force-frame-pointers=yes` (or don't pass `-C force-frame-pointers=no`),
/// otherwise `rbp` won't form a valid chain and this will print garbage/nothing.
fn print_stack_trace(writer: &mut PanicWriter, max_frames: usize) {
    let _ = writeln!(writer, "Stack trace:");

    let mut rbp: usize;
    unsafe {
        core::arch::asm!("mov {}, rbp", out(reg) rbp);
    }

    // Very rough sanity bounds for a typical higher-half kernel stack.
    // Adjust these to match your actual stack region if you have one.
    let is_plausible = |addr: usize| addr != 0 && addr % 8 == 0;

    let mut frame = rbp;
    for depth in 0..max_frames {
        if !is_plausible(frame) {
            break;
        }

        // Saved rbp is at [frame], return address is at [frame + 8].
        let saved_rbp_ptr = frame as *const usize;
        let ret_addr_ptr = (frame + 8) as *const usize;

        // We're in a panic handler with a possibly-corrupt stack, so read
        // defensively rather than trusting the pointers blindly.
        let ret_addr = unsafe { core::ptr::read_volatile(ret_addr_ptr) };
        let next_rbp = unsafe { core::ptr::read_volatile(saved_rbp_ptr) };

        if ret_addr == 0 {
            break;
        }

        // reorder to be addr2line direct copy
        let _ = writeln!(writer, "  {:#018x} #{}", ret_addr, depth);

        if next_rbp <= frame {
            // Frame pointers should increase as we walk toward the caller;
            // if they don't, the chain is broken (or we hit the bottom).
            break;
        }
        frame = next_rbp;
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { core::arch::asm!("cli") };

    let mut writer = PanicWriter { col: 0, row: 0 };
    writer.clear_screen();

    let _ = writeln!(writer, "KERNEL PANIC!");
    let _ = writeln!(writer);

    if let Some(location) = info.location() {
        let _ = writeln!(
            writer,
            "  at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }

    let _ = writeln!(writer, "  {}", info.message());
    let _ = writeln!(writer);

    print_stack_trace(&mut writer, 10);

    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
