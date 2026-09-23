/*
 * syscalls.h — the basicos syscall ABI, for freestanding C programs.
 *
 * No libc anywhere in this directory (see the top-level Makefile's
 * `-ffreestanding -nostdlib -nostdinc -static` and crt0.s's header
 * comment) — this header *is* the runtime's only connection to the
 * kernel. It defines its own minimal integer types instead of pulling
 * in <stdint.h>: with -nostdinc even the compiler's own freestanding
 * headers aren't on the include path, and there's no reason to fight
 * that for three typedefs.
 *
 * ABI (matches src/syscall/mod.rs and src/arch/x86_64/syscall.rs):
 *   RAX = syscall number, RDI/RSI/RDX/R10/R8/R9 = args 1-6,
 *   RAX = return value (negative = -errno, same convention as Linux).
 *   RCX/R11 are clobbered by the `syscall` instruction itself.
 *
 * IMPORTANT — this kernel's syscall return path does *not* restore
 * RDI/RSI/RDX/R8/R9/R10 the way real Linux does (see syscall.rs's
 * `syscall_return_to_user`: it only restores RBX/RBP/R12-R15/RCX/R11/
 * RSP). Treat every syscall as clobbering all six argument registers,
 * not just RCX/R11.
 *
 * The raw `__syscallN` wrappers below are marked `noinline, noipa` *and*
 * use "+" (read-write) constraints for every argument register — all of
 * that is load-bearing, not stylistic; `noinline` alone was tried first
 * and wasn't enough. See the long comment right above their
 * declarations for exactly what goes wrong without each piece — the
 * short version is that GCC can see straight through a `static`
 * function's body regardless of inlining, doesn't know this kernel's
 * syscall ABI clobbers more than a normal function call would, and (the
 * nastier failure) can clone a specialized copy per constant syscall
 * number that silently drops an argument load entirely.
 */

#ifndef BASICOS_SYSCALLS_H
#define BASICOS_SYSCALLS_H

typedef unsigned long uint64_t;
typedef long int64_t;
typedef unsigned int uint32_t;
typedef int int32_t;
typedef unsigned long size_t;
typedef long ssize_t;

#define NULL ((void *)0)

/* --- syscall numbers (matches syscall::nr) --- */
#define SYS_READ 0
#define SYS_WRITE 1
#define SYS_OPEN 2
#define SYS_CLOSE 3
#define SYS_LSEEK 8
#define SYS_MMAP 9
#define SYS_MUNMAP 11
#define SYS_BRK 12
#define SYS_YIELD 24
#define SYS_GETPID 39
#define SYS_FORK 57
#define SYS_EXECVE 59
#define SYS_EXIT 60
#define SYS_WAIT4 61
#define SYS_GETDENTS64 217
#define SYS_KLOG 0x400
#define SYS_TREE 0x401
#define SYS_KPANIC 0x402

/* --- open() flags (matches syscall::open_flags) --- */
#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR 2
#define O_CREAT 0100
#define O_TRUNC 01000
#define O_APPEND 02000
#define O_DIRECTORY 0200000

/* --- lseek() whence (matches syscall::whence) --- */
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

/* --- mmap() flags (matches syscall::mmap_flags) — anonymous only, see
 * sys_mmap's doc comment below. --- */
#define PROT_READ 0x1
#define PROT_WRITE 0x2
#define MAP_ANONYMOUS 0x20
#define MAP_FAILED ((void *)-1)

/* --- tree() targets (matches syscall::tree_target), basicos-specific --- */
#define TREE_DISK 0
#define TREE_INITRD 1
#define TREE_RAMDISK 2

/* Every wrapper below is `static`, so each .c file that includes this
 * header gets its own private copy (no multi-definition link errors
 * across translation units) — but that also means any program that
 * doesn't use, say, sys_tree() would otherwise get an "unused function"
 * warning for it under -Wall. This header is meant to be included
 * whole, like a real libc header, so that's expected noise, not a real
 * problem — silence it here rather than making every program's own
 * Makefile invocation carry `-Wno-unused-function`. Restored right
 * before the include guard closes, so a program's *own* unused static
 * functions still get warned about normally. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-function"

/*
 * Every argument register below is a "+" (read-write) operand, not a
 * plain input, even though the C code never looks at its value again
 * afterward. This is load-bearing, not stylistic — found the hard way
 * (see git history for the exact symptom: a later syscall in the same
 * function silently received a *previous* call's leftover argument
 * value instead of its own).
 *
 * These functions are `static`, fully visible in this translation
 * unit, and GCC's interprocedural analysis looks straight through a
 * call like that regardless of inlining. With RDI/RSI/RDX/R10/R8/R9 as
 * plain inputs, the compiler sees this asm declares no clobber on any
 * of them and concludes a caller's own unrelated value already sitting
 * in, say, RDX is still valid afterward — true on real Linux (this
 * exact pattern is musl's own raw-syscall idiom) but *not* true for
 * this kernel's syscall ABI: `syscall_return_to_user` only restores
 * RBX/RBP/R12-R15/RCX/R11/RSP, leaving every argument register as
 * whatever `dispatch()` happened to leave it (see this header's own top
 * comment). Marking them "+" tells the compiler each one holds an
 * unknown, different value on the way out, same as an explicit clobber
 * would — GCC doesn't allow literally clobbering a register that's
 * also bound to a named operand, so "+" (discarding the "output" side)
 * is the standard way to express it.
 *
 * `noipa` (not just `noinline`!) is just as load-bearing, for a nastier
 * reason: every one of these is called with a compile-time-constant
 * syscall number, so GCC's interprocedural constant propagation (part
 * of -O2) clones a specialized copy per distinct constant — e.g.
 * `__syscall1.constprop.0`, hardcoding `mov $3, %eax` for close's
 * SYS_CLOSE — and, having "proven" the discarded `+D(a1)` output is
 * dead in the clone, deletes the `mov` that was supposed to load `fd`
 * into RDI *entirely*, leaving the `syscall` instruction firing with
 * whatever RDI already held from the *previous unrelated call*. This
 * reproduced every time before `noipa`, always in the same shape: not
 * a crash, just a syscall silently getting some other call's leftover
 * argument. `noipa` disables that cloning — the C ABI's ordinary
 * ignores-unused-return-value dead-store elimination *doesn't* have
 * this bug, it's specific to IPA cloning interacting with hard-register
 * asm constraints, so nothing weaker than `noipa` reliably avoids it.
 */
static long __syscall0(long n) __attribute__((noinline, noipa));
static long __syscall1(long n, long a1) __attribute__((noinline, noipa));
static long __syscall2(long n, long a1, long a2) __attribute__((noinline, noipa));
static long __syscall3(long n, long a1, long a2, long a3) __attribute__((noinline, noipa));
static long __syscall4(long n, long a1, long a2, long a3, long a4)
    __attribute__((noinline, noipa));
static long __syscall5(long n, long a1, long a2, long a3, long a4, long a5)
    __attribute__((noinline, noipa));
static long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6)
    __attribute__((noinline, noipa));

static long __syscall0(long n) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(n) : "rcx", "r11", "memory");
    return ret;
}

static long __syscall1(long n, long a1) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret), "+D"(a1) : "a"(n) : "rcx", "r11", "memory");
    return ret;
}

static long __syscall2(long n, long a1, long a2) {
    long ret;
    __asm__ volatile("syscall"
                      : "=a"(ret), "+D"(a1), "+S"(a2)
                      : "a"(n)
                      : "rcx", "r11", "memory");
    return ret;
}

static long __syscall3(long n, long a1, long a2, long a3) {
    long ret;
    __asm__ volatile("syscall"
                      : "=a"(ret), "+D"(a1), "+S"(a2), "+d"(a3)
                      : "a"(n)
                      : "rcx", "r11", "memory");
    return ret;
}

static long __syscall4(long n, long a1, long a2, long a3, long a4) {
    long ret;
    register long r10 __asm__("r10") = a4;
    __asm__ volatile("syscall"
                      : "=a"(ret), "+D"(a1), "+S"(a2), "+d"(a3), "+r"(r10)
                      : "a"(n)
                      : "rcx", "r11", "memory");
    return ret;
}

static long __syscall5(long n, long a1, long a2, long a3, long a4, long a5) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8 __asm__("r8") = a5;
    __asm__ volatile("syscall"
                      : "=a"(ret), "+D"(a1), "+S"(a2), "+d"(a3), "+r"(r10), "+r"(r8)
                      : "a"(n)
                      : "rcx", "r11", "memory");
    return ret;
}

static long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8 __asm__("r8") = a5;
    register long r9 __asm__("r9") = a6;
    __asm__ volatile("syscall"
                      : "=a"(ret), "+D"(a1), "+S"(a2), "+d"(a3), "+r"(r10), "+r"(r8), "+r"(r9)
                      : "a"(n)
                      : "rcx", "r11", "memory");
    return ret;
}

/* --- typed wrappers --- */

static long sys_read(long fd, void *buf, size_t count) {
    return __syscall3(SYS_READ, fd, (long)buf, (long)count);
}

static long sys_write(long fd, const void *buf, size_t count) {
    return __syscall3(SYS_WRITE, fd, (long)buf, (long)count);
}

static long sys_open(const char *path, long flags, long mode) {
    return __syscall3(SYS_OPEN, (long)path, flags, mode);
}

static long sys_close(long fd) {
    return __syscall1(SYS_CLOSE, fd);
}

static long sys_lseek(long fd, int64_t offset, long whence) {
    return __syscall3(SYS_LSEEK, fd, (long)offset, whence);
}

static long sys_getdents64(long fd, void *dirp, size_t count) {
    return __syscall3(SYS_GETDENTS64, fd, (long)dirp, (long)count);
}

/* mmap: anonymous mappings only — there's no page cache in this kernel
 * to back a file-backed mapping with, so `fd` must be -1 and `flags`
 * must include MAP_ANONYMOUS or this returns MAP_FAILED. `addr` and
 * `offset` are accepted for ABI familiarity but ignored (the kernel
 * always picks the address itself). */
static void *sys_mmap(void *addr, size_t len, long prot, long flags, long fd, int64_t offset) {
    return (void *)__syscall6(SYS_MMAP, (long)addr, (long)len, prot, flags, fd, (long)offset);
}

static long sys_munmap(void *addr, size_t len) {
    return __syscall2(SYS_MUNMAP, (long)addr, (long)len);
}

/* brk: addr == NULL queries the current break. Always returns the
 * resulting break (never a negative errno — matches the real brk()
 * ABI: a failed grow just leaves the break where it was). */
static void *sys_brk(void *addr) {
    return (void *)__syscall1(SYS_BRK, (long)addr);
}

/* fork: returns the child's pid in the parent, 0 in the child, or a
 * negative errno on failure (e.g. the kernel's task table is full). */
static long sys_fork(void) {
    return __syscall0(SYS_FORK);
}

/* execve: replaces the calling process's own image. Does not return on
 * success — the process that called it is gone. `envp` is accepted for
 * ABI familiarity but ignored: there's no environment-variable support
 * in this kernel, every exec'd process gets an empty environment. */
static long sys_execve(const char *path, char *const argv[], char *const envp[]) {
    return __syscall3(SYS_EXECVE, (long)path, (long)argv, (long)envp);
}

/* wait4: options is accepted but ignored. pid > 0 waits for that
 * specific child; pid <= 0 waits for any child. Returns the reaped
 * child's pid, or a negative errno (ECHILD) if the caller has no
 * matching child at all. */
static long sys_wait4(long pid, int32_t *status, long options) {
    return __syscall3(SYS_WAIT4, pid, (long)status, options);
}

static void sys_exit(int status) __attribute__((noreturn));
static void sys_exit(int status) {
    __syscall1(SYS_EXIT, status);
    __builtin_unreachable();
}

static long sys_getpid(void) {
    return __syscall0(SYS_GETPID);
}

static long sys_yield(void) {
    return __syscall0(SYS_YIELD);
}

/* klog: write a message straight to the kernel log (basicos-specific,
 * no Linux equivalent). */
static long sys_klog(const char *buf, size_t len) {
    return __syscall2(SYS_KLOG, (long)buf, (long)len);
}

/* tree: recursively list a mounted filesystem to the kernel log
 * (basicos-specific) — target is one of TREE_DISK/TREE_INITRD/
 * TREE_RAMDISK. The walk happens in the kernel; this just reports
 * success/failure. */
static long sys_tree(long target) {
    return __syscall1(SYS_TREE, target);
}

/* kpanic: deliberately panics the kernel (basicos-specific, for testing
 * `panic.rs`). Never returns. */
static void sys_kpanic(void) __attribute__((noreturn));
static void sys_kpanic(void) {
    __syscall0(SYS_KPANIC);
    __builtin_unreachable();
}

#pragma GCC diagnostic pop

#endif /* BASICOS_SYSCALLS_H */
