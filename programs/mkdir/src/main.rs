//! mkdir <path>... — create directories.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: mkdir <path>...");
        return 2;
    }
    let mut status = 0;
    for path in args.iter().skip(1) {
        if let Err(e) = fs::create_dir(path) {
            eprintln!("mkdir: {}: {}", path, e);
            status = 1;
        }
    }
    status
}
