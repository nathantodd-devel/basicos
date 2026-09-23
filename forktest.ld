/* Linker script for forktest.s — a standalone ET_EXEC ELF64 binary.
 *
 * Same fixed load address as testprog.ld: 0x600000 is `PROC_CODE_START`,
 * the base of every process's own private code/data window (see
 * `mem::paging`'s module docs) — each process gets its own physical
 * backing there now, so two different binaries (or two instances of the
 * same one) both loading at this address don't collide.
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
