//! ls [path] — list a directory (default `C:/`). Exercises `fs::read_dir`,
//! which goes through `getdents64`, plus `Vec` and sorting from `alloc`.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(args: Args) -> Result<()> {
    let path = args.get(1).unwrap_or("C:/");
    let mut entries = fs::read_dir(path)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in &entries {
        println!("{}{}", e.name, if e.is_dir { "/" } else { "" });
    }
    println!("({} entries in {})", entries.len(), path);
    Ok(())
}
