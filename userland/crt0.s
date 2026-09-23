/*
 * crt0.s — the one piece of this directory that can't be C.
 *
 * A process's very first instruction can't be an ordinary C function:
 * the kernel hands control to `_start` with RSP pointing straight at
 * `argc` (then a NULL-terminated argv[] array, then a NULL-terminated
 * envp[] array — see sched::process's `setup_stack`), not at a return
 * address the way every C function assumes it was `call`ed. This reads
 * that layout, sets up a proper 16-byte-aligned stack, and only then
 * makes a real `call` into `main` — from that point on, everything is
 * ordinary C.
 *
 * `main`'s return value becomes the process's exit status, via a direct
 * `exit` syscall — not `syscalls.h`'s `sys_exit`: every wrapper in that
 * header is `static` (internal linkage, deliberately — see its header
 * comment) so C files can include it freely without multi-definition
 * errors, but that also means crt0.o, a *separate* object file, has no
 * externally-linked symbol to call. Simplest fix: just do the syscall
 * here directly instead of trying to share code across the link.
 */

.code64

.section .text
.global _start
.extern main
_start:
    xor %rbp, %rbp              # mark the end of the frame-pointer chain
    mov (%rsp), %rdi             # argc
    lea 8(%rsp), %rsi            # argv
    lea 8(%rsi,%rdi,8), %rdx     # envp = argv + (argc+1) pointers
    and $-16, %rsp                # align *before* the call — RSP is
                                   # 8-mod-16 here (see setup_stack), and
                                   # `call` below pushes 8 more bytes,
                                   # landing `main` at the usual 8-mod-16
                                   # "just entered a function" alignment
    call main

    mov %eax, %edi                # main's return value -> exit status
    mov $60, %rax                  # SYS_EXIT (syscalls.h)
    syscall

    # exit doesn't return, but just in case:
.hang:
    hlt
    jmp .hang
