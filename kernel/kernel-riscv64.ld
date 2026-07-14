/* RISC-V linker script for QEMU `virt` (S-mode via OpenSBI). OpenSBI jumps to the kernel at
 * 0x8020_0000. NS16550 UART at 0x1000_0000. No higher-half until the Sv39 MMU is set up. */

ENTRY(_start)

SECTIONS {
    . = 0x80200000;

    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
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

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.riscv.attributes) }
}
