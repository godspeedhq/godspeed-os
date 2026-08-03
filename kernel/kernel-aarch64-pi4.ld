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

    /* EL0 region, on its own 2 MiB boundary.
     *
     * This has to be a SEPARATE 2 MiB block from the kernel, not merely separate
     * bytes. In the EL1&0 translation regime, a region that is accessible from
     * EL0 is forced PXN - the kernel may not execute code that userspace can
     * reach (ARM's equivalent of x86 SMEP). So granting EL0 access to the block
     * holding the kernel's own .text makes the kernel non-executable at EL1, and
     * the core dies on its next instruction fetch with no way to report it: the
     * exception handler cannot fetch its vector either.
     *
     * That cost two hardware round trips. It presented first as `tlbi vmalle1`
     * hanging, and then as the MMU enable hanging - the same permission fault,
     * just landing at whichever point the new mapping took effect.
     *
     * The map uses 2 MiB blocks, so 2 MiB alignment is the granularity at which
     * EL0 access can be granted without dragging kernel code in with it. */
    . = ALIGN(0x200000);
    __el0_start = .;
    .el0 : ALIGN(0x1000) {
        *(.el0.text)
        *(.el0.data)
    }
    . = ALIGN(0x200000);
    __el0_end = .;

    /* End of everything the image occupies. `memmap` carves this range out of
     * usable RAM, so it must sit after the LAST thing the kernel owns - both the
     * boot stack and the EL0 region are above .bss, and a map built from
     * __bss_end alone would hand the allocator the stack it is standing on. */
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.ARM.attributes) }
}
