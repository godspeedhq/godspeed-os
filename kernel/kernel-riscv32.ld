/* RISC-V 32 linker script for QEMU `virt` (S-mode via OpenSBI) - the size study, not a port
 * (backlog/60). OpenSBI's 32-bit firmware jumps to the next stage at 0x8040_0000, where the 64-bit
 * one uses 0x8020_0000 (kernel-riscv64.ld). NS16550 UART at 0x1000_0000. Otherwise the same layout. */

ENTRY(_start)

SECTIONS {
    . = 0x80400000;

    __kernel_start = .;

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
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.riscv.attributes) }
}
