/* link.ld — linker script for init, a standalone ET_EXEC ELF64 binary.
 *
 * Load address 0x600000 is `mem::paging::PROC_CODE_START`, the base of
 * every process's own private code/data window in the kernel (same
 * value as ../userland/user.ld, ../testprog.ld, ../forktest.ld). The
 * kernel's loader (kernel/src/elf/mod.rs) requires every PT_LOAD
 * segment to sit entirely inside 0x600000..0xA00000 and does no
 * relocation, so this address is fixed, not a suggestion.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x600000;

    .text   : { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }
    .bss    : { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note.*)
    }
}
