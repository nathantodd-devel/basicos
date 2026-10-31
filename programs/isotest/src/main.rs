//! isotest — check that the kernel contains a misbehaving process: touching
//! kernel memory or NULL kills just that process, a failed exec leaves its
//! caller intact, and orphans go to init. Run by `make shell-test`.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

use core::time::Duration;
use process::Fork;

const SIGSEGV_STATUS: i32 = 128 + 11;

/// Run `f` in a child, which should die with a segfault.
fn must_fault(name: &str, f: fn()) -> bool {
    let child = match process::fork() {
        Ok(Fork::Child) => {
            f();
            println!("isotest: {}: VIOLATION, the access succeeded", name);
            process::exit(0);
        }
        Ok(Fork::Parent(pid)) => pid,
        Err(e) => {
            println!("isotest: fork: {}", e);
            return false;
        }
    };
    match process::wait(Some(child)) {
        Ok((_, SIGSEGV_STATUS)) => {
            println!("isotest: {}: blocked", name);
            true
        }
        other => {
            println!("isotest: {}: FAILED: {:?}", name, other);
            false
        }
    }
}

fn read_at(addr: usize) {
    unsafe { core::ptr::read_volatile(addr as *const u8) };
}

fn orphan() -> bool {
    let Ok(Fork::Parent(child)) = process::fork() else {
        // The middle process: leave a child behind and exit.
        if let Ok(Fork::Child) = process::fork() {
            while process::getppid() != 1 {
                process::yield_now();
            }
            println!("isotest: orphan adopted by init");
        }
        process::exit(0);
    };
    process::wait(Some(child)).is_ok()
}

fn main(_args: Args) -> i32 {
    let mut ok = true;
    ok &= must_fault("read NULL", || read_at(0));
    ok &= must_fault("read kernel image", || read_at(0x100000));
    ok &= must_fault("read VGA memory", || read_at(0xb8000));
    ok &= must_fault("write kernel image", || unsafe {
        core::ptr::write_volatile(0x100000 as *mut u8, 0)
    });
    ok &= must_fault("jump to kernel", || unsafe {
        let f: fn() = core::mem::transmute(0x100000usize);
        f();
    });

    // Not an ELF: exec must fail and leave us running.
    let err = process::exec("C:/hello.txt", &["hello"]);
    println!("isotest: exec of a non-ELF returned: {}", err);

    ok &= orphan();
    time::sleep(Duration::from_millis(300));

    if ok {
        println!("isotest: ALL PASSED");
        0
    } else {
        println!("isotest: FAILED");
        1
    }
}
