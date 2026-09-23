/*
 * entrypoint.s — Multiboot2 entry point.
 *
 * GRUB loads us in 32-bit protected mode with paging disabled.
 * This file:
 *   1. Advertises a Multiboot2 header so GRUB recognizes the kernel.
 *   2. Sanity-checks we were actually loaded by a Multiboot2 bootloader.
 *   3. Builds minimal page tables that identity-map the first 1 GiB.
 *   4. Enables PAE, long mode (EFER.LME), and paging.
 *   5. Loads a 64-bit GDT and far-jumps into long mode.
 *   6. Calls into Rust's kernel_main().
 */

.set MULTIBOOT2_MAGIC,        0xe85250d6
.set MULTIBOOT2_ARCH_I386,    0
.set MULTIBOOT2_HEADER_LEN,   multiboot_header_end - multiboot_header
.set MULTIBOOT2_CHECKSUM,     -(MULTIBOOT2_MAGIC + MULTIBOOT2_ARCH_I386 + MULTIBOOT2_HEADER_LEN)

.section .multiboot, "a"
.align 8
multiboot_header:
    .long MULTIBOOT2_MAGIC
    .long MULTIBOOT2_ARCH_I386
    .long MULTIBOOT2_HEADER_LEN
    .long MULTIBOOT2_CHECKSUM
    # end tag (type=0, flags=0, size=8)
    .word 0
    .word 0
    .long 8
multiboot_header_end:

.section .bss
.align 4096
p4_table:
    .skip 4096
p3_table:
    .skip 4096
p2_table:
    .skip 4096
.align 16
stack_bottom:
    .skip 65536          # 64 KiB stack
stack_top:

.section .rodata
gdt64:
    .quad 0                                   # null descriptor
.set gdt64.code, . - gdt64
    .quad (1<<43) | (1<<44) | (1<<47) | (1<<53)   # 64-bit code segment
gdt64_pointer:
    .word . - gdt64 - 1
    .quad gdt64

.section .text
.code32
.global _start
.type _start, @function
_start:
    mov $stack_top, %esp
    mov %esp, %ebp

    mov %eax, %edi        # save multiboot2 magic
    mov %ebx, %esi        # save multiboot2 info pointer

    call check_multiboot
    call check_cpuid
    call check_long_mode

    call setup_page_tables
    call enable_paging

    lgdt (gdt64_pointer)
    ljmp $gdt64.code, $long_mode_start

    # unreachable
    hlt

# --- sanity checks -----------------------------------------------------

check_multiboot:
    cmp $0x36d76289, %eax
    jne .no_multiboot
    ret
.no_multiboot:
    mov $'0', %al
    jmp error

check_cpuid:
    pushf
    pop %eax
    mov %eax, %ecx
    xor $(1 << 21), %eax
    push %eax
    popf
    pushf
    pop %eax
    push %ecx
    popf
    cmp %ecx, %eax
    je .no_cpuid
    ret
.no_cpuid:
    mov $'1', %al
    jmp error

check_long_mode:
    mov $0x80000000, %eax
    cpuid
    cmp $0x80000001, %eax
    jb .no_long_mode

    mov $0x80000001, %eax
    cpuid
    test $(1 << 29), %edx
    jz .no_long_mode
    ret
.no_long_mode:
    mov $'2', %al
    jmp error

# --- paging / long mode setup ------------------------------------------

setup_page_tables:
    # NOTE: every entry below is marked user-accessible (bit 2). That's a
    # bring-up simplification so ring-3 code can run inside the same
    # identity-mapped 1GiB range as the kernel — there's no real user/kernel
    # memory isolation yet. Once processes get their own address spaces,
    # kernel-only pages should go back to supervisor-only.

    # p4[0] -> p3_table
    mov $p3_table, %eax
    or $0b111, %eax          # present + writable + user
    mov %eax, (p4_table)

    # p3[0] -> p2_table
    mov $p2_table, %eax
    or $0b111, %eax
    mov %eax, (p3_table)

    # identity map 512 * 2MiB = 1 GiB using huge pages
    mov $0, %ecx
.map_p2_table:
    mov $0x200000, %eax     # 2MiB
    mul %ecx
    or $0b10000111, %eax    # present + writable + user + huge page
    mov %eax, p2_table(,%ecx,8)

    inc %ecx
    cmp $512, %ecx
    jne .map_p2_table

    ret

enable_paging:
    # load P4 into CR3
    mov $p4_table, %eax
    mov %eax, %cr3

    # enable PAE
    mov %cr4, %eax
    or $(1 << 5), %eax
    mov %eax, %cr4

    # set long mode bit in EFER MSR
    mov $0xC0000080, %ecx
    rdmsr
    or $(1 << 8), %eax
    wrmsr

    # enable paging
    mov %cr0, %eax
    or $(1 << 31), %eax
    mov %eax, %cr0

    ret

error:
    # print "ERR: X" to VGA text buffer and halt
    movl $0x4f524f45, (0xb8000)
    movl $0x4f3a4f52, (0xb8004)
    movl $0x4f204f20, (0xb8008)
    movb %al, (0xb800a)
    hlt

.code64
long_mode_start:
    # reload segment registers (data segments are ignored in long mode
    # but we clear them for sanity)
    xor %ax, %ax
    mov %ax, %ss
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %gs

    mov %rdi, %rdi         # multiboot magic  (arg 1)
    mov %rsi, %rsi         # multiboot info*  (arg 2)
    call kernel_main

.hang:
    cli
    hlt
    jmp .hang
