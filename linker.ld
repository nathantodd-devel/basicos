ENTRY(_start)

PHDRS
{
    boot   PT_LOAD FLAGS(5); /* R + X  — multiboot header, must be executable-segment-loadable */
    text   PT_LOAD FLAGS(5); /* R + X  — code */
    rodata PT_LOAD FLAGS(4); /* R only — read-only data */
    data   PT_LOAD FLAGS(6); /* R + W  — writable data */
}

SECTIONS
{
    /* GRUB loads the kernel at 1MiB by convention */
    . = 1M;

    __kernel_start = .;

    .boot :
    {
        /* multiboot header must be within the first 8KiB of the file */
        *(.multiboot)
    } :boot

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .text :
    {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata :
    {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data :
    {
        *(.data .data.*)
    } :data

    .bss :
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    __kernel_end = .;

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.comment)
        *(.note.*)
    }
}
