/*
 * forktest.s — exercises brk/mmap/munmap, fork, wait4, and execve from a
 * real ring-3 program: not part of the kernel image, assembled and
 * linked (via forktest.ld) into its own ET_EXEC binary, loaded onto the
 * FAT32 test disk, and run by the shell's `run` command — same idea as
 * testprog.s, just a heavier syscall workout.
 *
 * Talks to the kernel exactly like testprog.s does: raw `syscall`
 * instructions, same ABI (RAX=number, RDI/RSI/RDX/R10/R8/R9=args). No
 * libc, no relocations.
 *
 * IMPORTANT: like every syscall caller in this kernel, only RBX/RBP/
 * R12-R15/RSP survive a `syscall` unclobbered — RAX is the return value,
 * everything else (RDI/RSI/RDX/R8/R9/R10) is left as whatever dispatch()
 * happened to leave it in. Anything that needs to survive across a
 * syscall goes in a callee-saved register (r12-r14 below).
 */

.code64

.section .text
.global _start
_start:
    # --- brk: grow the heap by 8 KiB, prove the new page is real memory ---
    mov $12, %rax           # SYS_brk
    xor %rdi, %rdi           # 0 -> query current break
    syscall
    mov %rax, %r12           # r12 = initial brk

    mov %r12, %rdi
    add $8192, %rdi
    mov $12, %rax
    syscall                  # rax = new brk (should be >= r12+8192)

    movq $0x1eadbeef, (%r12)
    movq (%r12), %rax
    cmp $0x1eadbeef, %rax
    jne .Lfail

    # --- mmap: anonymous page, prove it's real (and independently) memory ---
    mov $9, %rax             # SYS_mmap
    xor %rdi, %rdi            # addr hint (ignored)
    mov $4096, %rsi
    mov $2, %rdx              # PROT_WRITE
    mov $0x20, %r10           # MAP_ANONYMOUS
    mov $-1, %r8              # fd = -1
    xor %r9, %r9              # offset
    syscall
    cmp $0, %rax
    jl .Lfail
    mov %rax, %r13            # r13 = mmap'd address
    movq $0x4afebabe, (%r13)
    movq (%r13), %rax
    cmp $0x4afebabe, %rax
    jne .Lfail

    mov %r13, %rdi             # munmap(addr, 4096)
    mov $4096, %rsi
    mov $11, %rax
    syscall

    lea msg_ok(%rip), %rsi
    mov $1, %rdi
    mov $msg_ok_len, %edx
    mov $1, %rax
    syscall

    # --- fork ---
    mov $57, %rax
    syscall
    cmp $0, %rax
    je .Lchild
    mov %rax, %r14             # r14 = child pid (parent only)

    lea msg_parent(%rip), %rsi
    mov $1, %rdi
    mov $msg_parent_len, %edx
    mov $1, %rax
    syscall

    mov %r14, %rdi              # wait4(child_pid, &status, 0)
    lea status(%rip), %rsi
    xor %rdx, %rdx
    mov $61, %rax
    syscall

    lea msg_reaped(%rip), %rsi
    mov $1, %rdi
    mov $msg_reaped_len, %edx
    mov $1, %rax
    syscall

    xor %rdi, %rdi
    mov $60, %rax               # exit(0)
    syscall

.Lchild:
    lea msg_child(%rip), %rsi
    mov $1, %rdi
    mov $msg_child_len, %edx
    mov $1, %rax
    syscall

    lea path(%rip), %rdi         # execve("C:/hello.elf", argv, NULL)
    lea argv(%rip), %rsi
    xor %rdx, %rdx
    mov $59, %rax
    syscall

    # only reached if execve failed
    lea msg_execfail(%rip), %rsi
    mov $1, %rdi
    mov $msg_execfail_len, %edx
    mov $1, %rax
    syscall
    mov $1, %rdi
    mov $60, %rax
    syscall

.Lfail:
    lea msg_fail(%rip), %rsi
    mov $1, %rdi
    mov $msg_fail_len, %edx
    mov $1, %rax
    syscall
    mov $1, %rdi
    mov $60, %rax
    syscall

.section .rodata
msg_ok:
    .ascii "forktest: brk/mmap/munmap ok\n"
.set msg_ok_len, . - msg_ok
msg_parent:
    .ascii "forktest: parent forked a child\n"
.set msg_parent_len, . - msg_parent
msg_child:
    .ascii "forktest: child about to execve\n"
.set msg_child_len, . - msg_child
msg_reaped:
    .ascii "forktest: parent reaped child\n"
.set msg_reaped_len, . - msg_reaped
msg_execfail:
    .ascii "forktest: execve failed\n"
.set msg_execfail_len, . - msg_execfail
msg_fail:
    .ascii "forktest: FAILED\n"
.set msg_fail_len, . - msg_fail
path:
    .asciz "C:/hello.elf"
.align 8
argv:
    .quad path
    .quad 0

.section .bss
.align 8
status:
    .skip 4
