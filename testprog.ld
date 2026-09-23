/* Linker script for testprog.s — a standalone ET_EXEC ELF64 binary.
 *
 * Fixed load address: there's no per-process paging yet, so
 * elf::load() copies PT_LOAD segments straight to their p_vaddr as a
 * physical address. 6 MiB is comfortably clear of the kernel image
 * (loaded at 1 MiB, currently well under 3 MiB) — elf::load() also
 * refuses to load anywhere at or below __kernel_end as a backstop.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x600000;

    .text : { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    .bss : { *(.bss .bss.*) }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.comment)
        *(.note.*)
    }
}
