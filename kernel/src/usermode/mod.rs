//! `init`, the one user-space program this kernel runs. It has no libc —
//! every syscall is invoked by hand — but the syscalls themselves are
//! Linux-shaped (`open`/`read`/`write`/`close`/`getdents64`, matching
//! ABI numbers and, for `getdents64`, the actual on-wire struct), so a
//! real C program's calls would look the same.

#[cfg(feature = "storage")]
use crate::syscall::open_flags;
use crate::syscall::{self, nr};

/// Scratch buffers for file reads and directory listings. Ring 3 gets a
/// 4 KiB stack (see `USER_STACK` in `lib.rs`), so these live in `.bss`
/// instead — user-accessible under the current bring-up page tables,
/// same as `USER_STACK` itself (see `entrypoint.s`).
#[cfg(feature = "storage")]
const FILE_BUF_LEN: usize = 256;
#[cfg(feature = "storage")]
const DIRENT_BUF_LEN: usize = 512;

#[cfg(feature = "storage")]
static mut FILE_BUF: [u8; FILE_BUF_LEN] = [0; FILE_BUF_LEN];
#[cfg(feature = "storage")]
static mut DIRENT_BUF: [u8; DIRENT_BUF_LEN] = [0; DIRENT_BUF_LEN];

fn write_bytes(bytes: &[u8]) {
    syscall::syscall3(nr::WRITE, 1, bytes.as_ptr() as u64, bytes.len() as u64);
}

/// `open()` + `read()` a whole file (up to `FILE_BUF`'s size) + `close()`,
/// printing what came back.
#[cfg(feature = "storage")]
fn demo_read_file(path: &[u8]) {
    let fd = syscall::syscall3(nr::OPEN, path.as_ptr() as u64, open_flags::O_RDONLY, 0);
    if fd < 0 {
        write_bytes(b"  (open failed)\n");
        return;
    }
    let buf_ptr = core::ptr::addr_of_mut!(FILE_BUF) as u64;
    let n = syscall::syscall3(nr::READ, fd as u64, buf_ptr, FILE_BUF_LEN as u64);
    if n > 0 {
        let slice = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, n as usize) };
        write_bytes(b"  ");
        write_bytes(slice);
        write_bytes(b"\n");
    }
    syscall::syscall1(nr::CLOSE, fd as u64);
}

/// `open()` + `lseek()` + `read()` + `close()` — reposition the cursor
/// before reading, instead of always reading from the start.
#[cfg(feature = "storage")]
fn demo_seek_read(path: &[u8], offset: i64, whence: u64) {
    let fd = syscall::syscall3(nr::OPEN, path.as_ptr() as u64, open_flags::O_RDONLY, 0);
    if fd < 0 {
        write_bytes(b"  (open failed)\n");
        return;
    }
    syscall::syscall3(nr::LSEEK, fd as u64, offset as u64, whence);
    let buf_ptr = core::ptr::addr_of_mut!(FILE_BUF) as u64;
    let n = syscall::syscall3(nr::READ, fd as u64, buf_ptr, FILE_BUF_LEN as u64);
    if n > 0 {
        let slice = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, n as usize) };
        write_bytes(b"  ");
        write_bytes(slice);
        write_bytes(b"\n");
    }
    syscall::syscall1(nr::CLOSE, fd as u64);
}

/// `open(O_DIRECTORY)` + `getdents64()` in a loop (exactly how glibc's
/// `readdir()` works under the hood) + `close()`, printing each name.
#[cfg(feature = "storage")]
fn demo_list_dir(path: &[u8]) {
    let fd = syscall::syscall3(
        nr::OPEN,
        path.as_ptr() as u64,
        open_flags::O_RDONLY | open_flags::O_DIRECTORY,
        0,
    );
    if fd < 0 {
        write_bytes(b"  (opendir failed)\n");
        return;
    }

    let buf_ptr = core::ptr::addr_of_mut!(DIRENT_BUF) as u64;

    loop {
        let n = syscall::syscall3(nr::GETDENTS64, fd as u64, buf_ptr, DIRENT_BUF_LEN as u64);
        if n <= 0 {
            break; // 0 = end of directory, <0 = error
        }
        let bytes = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, n as usize) };
        let mut offset = 0usize;
        while offset + 19 <= bytes.len() {
            // linux_dirent64: d_ino(8) d_off(8) d_reclen(2) d_type(1) d_name[]
            let reclen = u16::from_le_bytes([bytes[offset + 16], bytes[offset + 17]]) as usize;
            let d_type = bytes[offset + 18];
            let name_start = offset + 19;
            let name_end = bytes[name_start..(offset + reclen).min(bytes.len())]
                .iter()
                .position(|&b| b == 0)
                .map_or(offset + reclen, |p| name_start + p);

            write_bytes(b"  ");
            write_bytes(&bytes[name_start..name_end]);
            if d_type == 4 {
                // DT_DIR
                write_bytes(b"/");
            }
            write_bytes(b"\n");

            if reclen == 0 {
                break; // malformed record — avoid spinning forever
            }
            offset += reclen;
        }
    }

    syscall::syscall1(nr::CLOSE, fd as u64);
}

/// `open(O_CREAT|O_WRONLY|O_TRUNC)` + `write()` + `close()` — the write
/// only actually reaches the FAT32 volume when `close()` flushes it
/// (see `syscall::sys_close`).
#[cfg(feature = "storage")]
fn demo_write_file(path: &[u8], contents: &[u8]) {
    let fd = syscall::syscall3(
        nr::OPEN,
        path.as_ptr() as u64,
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC,
        0,
    );
    if fd < 0 {
        write_bytes(b"  (open for write failed)\n");
        return;
    }
    syscall::syscall3(
        nr::WRITE,
        fd as u64,
        contents.as_ptr() as u64,
        contents.len() as u64,
    );
    syscall::syscall1(nr::CLOSE, fd as u64);
}

pub extern "C" fn user_mode_stub() -> ! {
    // Use syscall to write to stdout
    const MSG: &[u8] = b"Hello from ring 3 via syscall!\n";
    syscall::syscall3(nr::WRITE, 1, MSG.as_ptr() as u64, MSG.len() as u64);

    // Write to kernel log
    const LOG_MSG: &[u8] = b"User process started successfully";
    syscall::syscall2(nr::KLOG, LOG_MSG.as_ptr() as u64, LOG_MSG.len() as u64);

    // Get our PID
    let pid = syscall::syscall0(nr::GETPID);

    // Format and print PID (simple, no formatting library in userspace)
    const PID_MSG: &[u8] = b"My PID: ";
    write_bytes(PID_MSG);
    let pid_char = b'0' + (pid as u8); // works for single digit
    write_bytes(&[pid_char]);
    write_bytes(b"\n");

    // Ask the kernel to walk both mounted filesystems and log a full,
    // recursive tree of their contents (basicos-specific — see
    // `syscall::sys_tree`; ring 3 has no port I/O or fs access of its
    // own, so the walk happens in kernel mode). Storage-only: without
    // it, `nr::TREE` isn't handled and this would just get ENOSYS back.
    #[cfg(feature = "storage")]
    {
        write_bytes(b"Requesting a tree of the disk and initrd...\n");
        syscall::syscall1(nr::TREE, syscall::tree_target::DISK);
        syscall::syscall1(nr::TREE, syscall::tree_target::INITRD);

        // The Linux-shaped way: open/read/getdents64/write/close,
        // driven entirely from here in ring 3.
        write_bytes(b"open+read /hello.txt:\n");
        demo_read_file(b"/hello.txt\0");

        write_bytes(b"open+read /initrd/hello.txt:\n");
        demo_read_file(b"/initrd/hello.txt\0");

        write_bytes(b"open+lseek(SEEK_SET, 6)+read /hello.txt:\n");
        demo_seek_read(b"/hello.txt\0", 6, syscall::whence::SEEK_SET);

        write_bytes(b"opendir+getdents64 /:\n");
        demo_list_dir(b"/\0");

        write_bytes(b"opendir+getdents64 /initrd:\n");
        demo_list_dir(b"/initrd\0");

        write_bytes(
            b"open(O_CREAT|O_WRONLY|O_TRUNC)+write+close /greeting.txt, then read it back:\n",
        );
        demo_write_file(
            b"/greeting.txt\0",
            b"written via open/write/close from ring 3",
        );
        demo_read_file(b"/greeting.txt\0");
    }

    // Deliberately not here: directly calling `alloc::vec::Vec`/`Box`/etc
    // from this function. `mem::heap`'s global allocator is protected by
    // a `Spinlock` that unconditionally executes `cli`/`sti` — privileged
    // instructions, illegal below CPL 0 — and this function always runs
    // at ring 3 in the end. That's true even when it's spawned as a
    // `sched` task rather than reached via `gdt::jump_to_ring3` directly:
    // its first `syscall` returns via `sysretq`, which forces CPL 3
    // unconditionally (the hardcoded ring-3 selectors in `STAR` — see
    // `arch::x86_64::syscall::init`), regardless of which ring made the
    // call. So a heap access placed here doesn't fail loudly at the
    // point it's spawned wrong; it runs fine until the first real
    // allocation, then `cli` inside `Spinlock::lock()` takes a #GP(0)
    // ("privileged instruction") that looks nothing like the actual
    // problem. If you want to exercise the allocator, do it somewhere
    // that's genuinely ring 0 — a `sched` task, or the shell (see
    // `shell::cmd_heapfill`) — the same way file I/O here goes through a
    // syscall instead of touching `fs::Fat32Fs` directly.

    syscall::syscall0(nr::KPANIC);
    syscall::syscall1(nr::EXIT, 0);

    unreachable!()
}
