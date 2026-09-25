//! heap — stress test for the SDK's global allocator. Checks the things
//! a free-list heap most easily gets wrong: growth (Vec reallocating
//! repeatedly), alignment, reuse of freed memory (a loop that would run
//! out of address space if free leaked), and coalescing (freeing many
//! small blocks must let one big one fit again).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(_args: Args) -> i32 {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool| {
        println!("heap: {:<28} {}", name, if ok { "ok" } else { "FAILED" });
        if !ok {
            failures += 1;
        }
    };

    // Growth: 100k u64s is ~800 KiB, reached through ~17 doublings.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..100_000u64 {
        v.push(i * 3);
    }
    check("vec growth + contents", v.iter().enumerate().all(|(i, &x)| x == i as u64 * 3));
    drop(v);

    // Strings and formatting.
    let mut s = String::new();
    for i in 0..500 {
        s.push_str(&format!("{},", i));
    }
    check("string building", s.len() > 1000 && s.starts_with("0,1,2,"));

    // Boxes and BTreeMap (lots of small, mixed-size allocations).
    let mut m = BTreeMap::new();
    for i in 0..2000u32 {
        m.insert(i, Box::new([i; 4]));
    }
    check("btreemap of boxes", m.get(&1234).map(|b| b[3]) == Some(1234));
    drop(m);

    // Alignment: over-aligned allocations must actually be aligned.
    #[repr(align(256))]
    struct Big([u8; 300]);
    let bx = Box::new(Big([7; 300]));
    check("256-byte alignment", (&*bx as *const Big as usize) % 256 == 0 && bx.0[299] == 7);
    drop(bx);

    // Reuse: 2000 x 64 KiB = 128 MiB of total allocation, but never more
    // than 64 KiB live. Only survives if freed blocks are reused (the
    // process's heap + mmap windows together are ~11 MiB).
    let mut reuse_ok = true;
    for i in 0..2000usize {
        // `vec![x; n]` (memset), not `resize` — which is a per-element
        // loop, painfully slow in a debug build under emulation.
        let b = vec![(i & 0xff) as u8; 64 * 1024];
        reuse_ok &= b[12345] == (i & 0xff) as u8;
    }
    check("reuse (128 MiB churn)", reuse_ok);

    // Coalescing: fill the heap with 4 KiB blocks, free them all, then a
    // single 3 MiB block must fit — only possible if they merged back.
    let mut small: Vec<Vec<u8>> = Vec::new();
    for _ in 0..1000 {
        small.push(vec![1u8; 4096]);
    }
    drop(small);
    let big = vec![9u8; 3 * 1024 * 1024];
    check("coalescing (3 MiB after 4 MiB)", big[3 * 1024 * 1024 - 1] == 9);

    if failures == 0 {
        println!("heap: all checks passed");
    }
    failures
}
