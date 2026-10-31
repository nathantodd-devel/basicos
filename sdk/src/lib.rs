//! `basicos-sdk` — the Rust userspace SDK for basicos.
//!
//! Everything a `#![no_std]` Rust program needs to run on this kernel:
//! syscall wrappers ([`fs`], [`process`], [`mem`], [`io`]; raw
//! primitives in [`sys`]), a global allocator ([`heap`], so `Vec`,
//! `String`, `Box` and `format!` work), the [`entry!`] macro that emits
//! the `_start` shim, a panic handler, and `print!`/`println!`.
//!
//! A program is a crate in `programs/` (`make new-prog NAME=foo` scaffolds
//! one) that depends on this SDK:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! use basicos_sdk::prelude::*;
//!
//! basicos_sdk::entry!(main);
//!
//! fn main(args: Args) {
//!     println!("hello from pid {}", process::getpid());
//! }
//! ```
//!
//! The rest of the toolchain — the target spec, linker script, and cargo
//! settings — is `config.toml` and `link.ld` in this crate's directory;
//! the top-level Makefile passes the former to cargo with `--config`.
//! There is no `std` for basicos: this SDK is `core` + `alloc` + the
//! kernel's syscalls, which is what a `std` port would be built on.
//!
//! What the target enforces: no SSE/x87/MMX (the scheduler
//! doesn't save FPU/vector state, so floating point is soft-float), no
//! red zone, `panic = "abort"`, static non-PIE code at a fixed load
//! address (the kernel's ELF loader does no relocation).

#![no_std]

extern crate alloc;

pub mod errno;
pub mod fs;
pub mod heap;
pub mod io;
pub mod mem;
pub mod net;
pub mod process;
pub mod rt;
pub mod sys;
pub mod time;

pub use errno::{Errno, Result};

/// `use basicos_sdk::prelude::*;` for the usual imports.
pub mod prelude {
    pub use crate::errno::{Errno, Result};
    pub use crate::rt::Args;
    pub use crate::{eprint, eprintln, fs, io, mem, net, print, println, process, time};
    pub use alloc::boxed::Box;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec::Vec;
    pub use alloc::{format, vec};
}
