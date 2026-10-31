//! mv <from> <to> — move or rename, within one drive.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    let (Some(from), Some(to)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: mv <from> <to>");
        return 2;
    };
    match fs::rename(from, to) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mv: {}: {}", from, e);
            1
        }
    }
}
