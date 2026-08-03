/* AArch64 linker script for the Raspberry Pi 4 Model B (BCM2711, Cortex-A72).
 *
 * The VideoCore firmware loads the 64-bit kernel image as a FLAT BINARY at
 * 0x80000 and branches to the first byte, so `_start` must be physically first
 * in the image. `.text.boot` + KEEP + ENTRY pin it there. Note this is 0x80000,
 * NOT the 32-bit port's 0x8000 - the 64-bit entry address is different.
 *
 * No higher-half: the MMU is off at boot, so we run identity-mapped at this low
 * physical address until page tables exist.
 *
 * This is the stock-firmware boot path (docs/aarch64.md §5, "bare GPU bootloader
 * + DTB"), chosen over UEFI+Limine because it needs nothing on the card beyond
 * the image itself. It does not foreclose the UEFI route: that would be a
 * different linker script and entry, not a change to anything above the arch
 * layer.
 */

ENTRY(_start)

SECTIONS {
    . = 0x80000;

    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))     /* _start first - the firmware branches to byte 0 */
        *(.text .text.*)
    }

    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) }
    .data   : ALIGN(0x1000) { *(.data .data.*) }

    . = ALIGN(16);
    __bss_start = .;
    .bss : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(16);
    __bss_end = .;

    /* 512 KiB boot stack, top-aligned. AAPCS64 requires SP 16-byte aligned; the
     * ALIGN(16) above plus a multiple-of-16 size keeps __stack_top aligned. */
    . = ALIGN(16);
    . += 0x80000;
    __stack_top = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.ARM.attributes) }
}
