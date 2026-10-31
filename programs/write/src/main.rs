//! write <path> <text...> — create or overwrite a file with the text.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> i32 {
    let Some(path) = args.get(1).filter(|_| args.len() > 2) else {
        eprintln!("usage: write <path> <text...>");
        return 2;
    };
    let words: Vec<&str> = args.iter().skip(2).collect();
    let text = words.join(" ");
    match fs::write(path, text.as_bytes()) {
        Ok(()) => {
            println!("wrote {} bytes to {}", text.len(), path);
            0
        }
        Err(e) => {
            eprintln!("write: {}: {}", path, e);
            1
        }
    }
}
