//! init — the first userspace program, meant to run as a long-lived
//! process that reaps orphaned children.
//!
//! A `#![no_std]` Rust program built on `basicos-sdk` (see `sdk/`): the
//! SDK supplies the `_start` shim, syscalls, and panic handler, and the
//! toolchain config (target, linker script, load address) lives in
//! `sdk/config.toml` + `sdk/link.ld`. Built via `make init-elf`.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

use basicos_sdk::process::Fork::{Child, Parent};
use basicos_sdk::process::{Fork, fork};

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    // init process
    println!(
        "init: started as pid {} (argc={})",
        process::getpid(),
        args.len()
    );
    for (i, a) in args.iter().enumerate() {
        println!("init: argv[{}] = {}", i, a);
    }

    // Reaper loop: collect any child that exits, forever. `wait` fails
    // with ECHILD immediately when there are no children at all, so yield
    // in that case rather than spinning flat out.
    loop {
        match process::wait(None) {
            Ok((pid, status)) => println!("init: reaped pid {} (status {})", pid, status),
            Err(_) => process::yield_now(),
        }
    }
}
