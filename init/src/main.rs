//! init — the first userspace program, pid 1. It starts the shell on the
//! console, starts a new one whenever that exits, and reaps every orphan the
//! kernel hands it. If init ever exits the kernel panics, so nothing here
//! returns.
//!
//! A `#![no_std]` Rust program built on `basicos-sdk` (see `sdk/`): the
//! SDK supplies the `_start` shim, syscalls, and panic handler, and the
//! toolchain config (target, linker script, load address) lives in
//! `sdk/config.toml` + `sdk/link.ld`. Built via `make init-elf`.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

use core::time::Duration;
use process::{Fork, Pid};

basicos_sdk::entry!(main);

const SHELL: &str = "C:/BIN/BASH.ELF";

/// A shell that dies sooner than this is failing, not being logged out of,
/// so wait a moment before starting another one.
const MIN_SHELL_LIFE: Duration = Duration::from_secs(1);

fn start_shell() -> Option<Pid> {
    match process::fork() {
        Ok(Fork::Child) => {
            let err = process::exec(SHELL, &["bash"]);
            eprintln!("init: can't start {}: {}", SHELL, err);
            process::exit(127);
        }
        Ok(Fork::Parent(pid)) => Some(pid),
        Err(e) => {
            eprintln!("init: fork: {}", e);
            None
        }
    }
}

fn main(_args: Args) -> i32 {
    println!("init: started as pid {}", process::getpid());
    let mut shell = start_shell();
    let mut started = time::Instant::now();

    loop {
        match process::wait(None) {
            Ok((pid, _)) if Some(pid) == shell => {
                if started.elapsed() < MIN_SHELL_LIFE {
                    time::sleep(MIN_SHELL_LIFE);
                }
                shell = start_shell();
                started = time::Instant::now();
            }
            Ok((pid, status)) => println!("init: reaped orphan {} (status {})", pid, status),
            // No children at all: nothing to reap, and the fork failed.
            Err(_) => {
                time::sleep(MIN_SHELL_LIFE);
                shell = start_shell();
                started = time::Instant::now();
            }
        }
    }
}
