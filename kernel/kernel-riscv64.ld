/* RISC-V linker script for QEMU `virt` (S-mode via OpenSBI). OpenSBI jumps to the kernel at
 * 0x8020_0000. NS16550 UART at 0x1000_0000. No higher-half until the Sv39 MMU is set up. */

ENTRY(_start)

SECTIONS {
    . = 0x80200000;

    /* The kernel's own span, so the frame allocator can refuse to hand out the memory it is
       running from. Taken from the link, never from a constant: the two boards load at different
       addresses and the allocator must not need to know which one it is on. */
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
    /* End of everything the image occupies, INCLUDING the boot stack. A frame allocator handed a
       smaller span would cheerfully allocate the stack out from under the caller. */
    __kernel_end = .;

    /* Total span the loader must keep clear for us, for the Image header's `image_size`.
       Measured from the load address to the end of the stack, so it covers text, rodata,
       data, bss AND the boot stack - a loader that placed something at __stack_top - 8
       would corrupt us before the first push. */
    __image_size = . - 0x80200000;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.riscv.attributes) }
}
