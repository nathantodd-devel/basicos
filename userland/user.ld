/* user.ld — shared linker script for every program in this directory.
 *
 * Same fixed load address as ../testprog.ld/../forktest.ld: 0x600000 is
 * `mem::paging::PROC_CODE_START`, the base of every process's own
 * private code/data window in the kernel. Each process gets its own
 * physical backing there now (see that module's doc comment), so every
 * program here can share this exact same address without colliding —
 * there's no relocation support either way (see ../src/elf/mod.rs), so
 * it has to be a fixed address regardless.
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
