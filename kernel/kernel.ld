ENTRY(_start)

SECTIONS {
    /* Limine loads the kernel at the physical address of its ELF LOAD segments.
       0x100000 (1 MiB) keeps us above the real-mode IVT and BIOS data areas. */
    . = 0x100000;

    /* Limine request structs — must be in a named section so KEEP preserves them.
       RequestsStartMarker and RequestsEndMarker bracket the requests region. */
    .requests : {
        KEEP(*(.requests_start))
        KEEP(*(.requests .requests.*))
        KEEP(*(.requests_end))
    }

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    .rodata : ALIGN(0x1000) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(0x1000) {
        *(.data .data.*)
    }

    .bss : ALIGN(0x1000) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
        *(.comment)
    }
}
