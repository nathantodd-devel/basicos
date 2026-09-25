//! hello — the smallest useful program: greets, and echoes its argv (so
//! it doubles as a check that arguments survive `exec`).

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) {
    println!("hello from Rust, pid {}!", process::getpid());
    for (i, a) in args.iter().enumerate() {
        println!("  argv[{}] = {}", i, a);
    }
}
