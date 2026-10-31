//! cat <path>... — print files.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: cat <path>...");
        return 2;
    }
    let mut status = 0;
    for path in args.iter().skip(1) {
        match fs::read_to_string(path) {
            Ok(text) if text.ends_with('\n') => print!("{}", text),
            Ok(text) => println!("{}", text),
            Err(e) => {
                eprintln!("cat: {}: {}", path, e);
                status = 1;
            }
        }
    }
    status
}
