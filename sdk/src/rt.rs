//! Program startup and shutdown: the pieces `entry!` and the panic
//! handler are built from.

use crate::errno::Errno;
use crate::{eprintln, process};

/// The program's command line (`argv`), as the kernel laid it out on the
/// stack at process entry.
#[derive(Clone, Copy)]
pub struct Args {
    argc: usize,
    argv: *const *const u8,
}

impl Args {
    #[doc(hidden)]
    pub unsafe fn from_raw(argc: usize, argv: *const *const u8) -> Args {
        Args { argc, argv }
    }

    pub fn len(&self) -> usize {
        self.argc
    }

    pub fn is_empty(&self) -> bool {
        self.argc == 0
    }

    /// `argv[i]`, if present (and valid UTF-8).
    pub fn get(&self, i: usize) -> Option<&'static str> {
        if i >= self.argc {
            return None;
        }
        unsafe {
            let p = *self.argv.add(i);
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            core::str::from_utf8(core::slice::from_raw_parts(p, len)).ok()
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static str> + use<> {
        let a = *self;
        (0..a.argc).filter_map(move |i| a.get(i))
    }
}

/// What `main` may return; becomes the process's exit status.
pub trait Termination {
    fn report(self) -> i32;
}

impl Termination for () {
    fn report(self) -> i32 {
        0
    }
}

impl Termination for i32 {
    fn report(self) -> i32 {
        self
    }
}

impl Termination for Result<(), Errno> {
    fn report(self) -> i32 {
        match self {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {}", e);
                1
            }
        }
    }
}

#[doc(hidden)]
pub fn run<T: Termination>(main: fn(Args) -> T, argc: usize, argv: *const *const u8) -> i32 {
    main(unsafe { Args::from_raw(argc, argv) }).report()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    eprintln!("panic: {}", info);
    process::exit(101)
}

/// Declare a program's entry point:
///
/// ```ignore
/// #![no_std]
/// #![no_main]
/// use basicos_sdk::prelude::*;
///
/// basicos_sdk::entry!(main);
///
/// fn main(args: Args) -> i32 {
///     println!("hello, {} args", args.len());
///     0
/// }
/// ```
///
/// `main` takes [`Args`] and may return `()`, an `i32` exit status, or
/// `Result<(), Errno>` (an `Err` is printed and exits with status 1).
///
/// This emits the `_start` shim into *your* crate, not the SDK's. A
/// process's first instruction can't be an ordinary function: the kernel
/// enters `_start` with RSP pointing straight at `argc` (then argv[],
/// then envp[] — see `sched::process::setup_stack`), not at a return
/// address. The shim reads that layout into the C ABI argument registers,
/// re-aligns the stack, and only then makes a real `call`. (Defining it
/// in the SDK would work only if the linker happened to pull that archive
/// member in; emitting it here means it's always in the final link.)
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        ::core::arch::global_asm!(
            ".globl _start",
            "_start:",
            "xor ebp, ebp",               // end of the frame-pointer chain
            "mov rdi, [rsp]",             // argc
            "lea rsi, [rsp + 8]",         // argv
            "lea rdx, [rsi + rdi*8 + 8]", // envp = argv + (argc + 1) pointers
            "and rsp, -16",               // RSP is 8-mod-16 here; `call` restores the usual alignment
            "call {shim}",
            "mov edi, eax",               // exit status
            "mov eax, 60",                // SYS_EXIT
            "syscall",
            "2: hlt",                     // exit doesn't return, but just in case
            "jmp 2b",
            shim = sym __basicos_main,
        );

        #[unsafe(no_mangle)]
        pub extern "C" fn __basicos_main(
            argc: usize,
            argv: *const *const u8,
            _envp: *const *const u8,
        ) -> i32 {
            $crate::rt::run($main, argc, argv)
        }
    };
}
