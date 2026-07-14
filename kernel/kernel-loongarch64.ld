/* LoongArch64 linker script for QEMU `virt`. DA mode at reset (VA==PA). NS16550 UART @ 0x1fe0_01e0. */
ENTRY(_start)
SECTIONS {
    . = 0x200000;
    .text : ALIGN(0x1000) { KEEP(*(.text.boot)) *(.text .text.*) }
    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) *(.srodata .srodata.*) }
    .data   : ALIGN(0x1000) { *(.data .data.*) *(.sdata .sdata.*) }
    . = ALIGN(16);
    __bss_start = .;
    .bss : { *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON) }
    . = ALIGN(16);
    __bss_end = .;
    . = ALIGN(16);
    . += 0x80000;
    __stack_top = .;
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.LoongArch.attributes) }
}
