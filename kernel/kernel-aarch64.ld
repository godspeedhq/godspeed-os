/* AArch64 linker script for QEMU `virt` (docs/aarch64.md, boot bring-up).
 * `qemu-system-aarch64 -M virt -kernel` loads the image at 0x4008_0000 and
 * enters at the ELF entry (_start) in EL1/EL2. No higher-half yet (MMU off at
 * boot); we run at the physical/identity low address until the MMU is set up. */

ENTRY(_start)

SECTIONS {
    . = 0x40080000;

    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))     /* _start first */
        *(.text .text.*)
    }

    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) }
    .data   : ALIGN(0x1000) { *(.data .data.*) }

    . = ALIGN(16);
    __bss_start = .;
    .bss : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(16);
    __bss_end = .;

    /* 512 KiB boot stack, top-aligned. */
    . = ALIGN(16);
    . += 0x80000;
    __stack_top = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) }
}
