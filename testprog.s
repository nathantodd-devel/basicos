/*
 * testprog.s — tiny standalone ELF64 executable for exercising
 * `elf::load`/`elf::run`.
 *
 * Not part of the kernel image: assembled and linked (via testprog.ld)
 * into its own ET_EXEC binary, loaded onto the FAT32 test disk, and
 * loaded+jumped-to by the kernel's ELF loader — proving the whole path
 * end to end (parse -> copy PT_LOAD segments -> jump to entry -> real
 * syscalls from a binary that was never linked into the kernel).
 *
 * Talks to the kernel exactly like usermode/mod.rs does: raw `syscall`
 * instructions, same ABI (RAX=number, RDI/RSI/RDX/R10=args). No libc,
 * no relocations — matches what the loader supports (see elf/mod.rs).
 */

.code64

.section .text
.global _start
_start:
    # write(1, msg, msglen)
    mov $1, %rax            # SYS_write
    mov $1, %rdi            # fd = stdout
    lea msg(%rip), %rsi
    mov $msglen, %edx
    syscall

    # exit(0)
    mov $60, %rax           # SYS_exit
    xor %rdi, %rdi
    syscall

    # exit() doesn't return, but just in case:
.hang:
    hlt
    jmp .hang

.section .rodata
msg:
    .ascii "Hello from a loaded ELF binary!\n"
.set msglen, . - msg
