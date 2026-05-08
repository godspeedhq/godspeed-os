ENTRY(kernel_main)

SECTIONS {
    /* Load at 1 MiB — standard multiboot2 load address */
    . = 0x100000;

    .text : {
        KEEP(*(.text.kernel_main))
        KEEP(*(.text.ap_main))
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

    /* Discard metadata sections that cause linker warnings on bare metal */
    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
        *(.comment)
    }
}
