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

/* The kernel is LINKED high and LOADED low.
 *
 * VMA = KERNEL_VA + 0x80000, LMA = 0x80000. The firmware still drops a flat
 * binary at physical 0x80000 and branches to byte 0 (objcopy -O binary emits by
 * LMA), but every symbol address is a high, TTBR1 address.
 *
 * Why this works across the transition, which is the part worth understanding:
 *
 *  - Before the MMU is on, `_start` reaches symbols only through `adrp`/`add`,
 *    which are PURELY PC-RELATIVE. The linker computes the delta between two
 *    high addresses; adding that delta to a low PC yields the correct LOW
 *    physical address. So the same instruction is right in both worlds.
 *  - Once the MMU is on with TTBR0 = identity AND TTBR1 = high, BOTH address
 *    ranges translate at the same time. That overlap is what makes the jump to
 *    the high half safe: there is no instant at which the code is running from
 *    an address that does not resolve.
 *  - After the jump, TTBR0 is free for a task, which is the entire point.
 *
 * The dangerous case is code running BEFORE the MMU that materialises a symbol
 * address absolutely (movz/movk) rather than PC-relatively - that yields a high
 * address with no translation yet. `relocation-model=static` with the small code
 * model emits adrp for statics, which is why this holds, but it is an assumption
 * about codegen rather than a guarantee, so the early boot deliberately prints
 * as soon as it possibly can.
 */
KERNEL_VA = 0xFFFFFF8000000000;

SECTIONS {
    . = KERNEL_VA + 0x80000;

    .text : AT(ADDR(.text) - KERNEL_VA) ALIGN(0x1000) {
        KEEP(*(.text.boot))     /* _start first - the firmware branches to byte 0 */
        *(.text .text.*)
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VA) ALIGN(0x1000) { *(.rodata .rodata.*) }
    .data   : AT(ADDR(.data)   - KERNEL_VA) ALIGN(0x1000) { *(.data .data.*) }

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
    .el0 : AT(ADDR(.el0) - KERNEL_VA) ALIGN(0x1000) {
        *(.el0.text)
        *(.el0.data)
    }
    . = ALIGN(0x200000);
    __el0_end = .;

    /* .bss and the boot stack come AFTER the EL0 region, and that ordering is
     * about image SIZE, not addresses.
     *
     * `objcopy -O binary` emits every byte from the load address up to the last
     * allocated section, so anything placed beyond .bss drags .bss into the file
     * as literal zeros. That went unnoticed while .bss was a few KiB of arch
     * state; the moment the neutral scheduler linked in (its per-task slots and
     * routing queues are large static arrays) the image jumped from 1.5 MB to
     * 18 MB of mostly zeros - which the firmware then has to read off the card
     * on every boot.
     *
     * With .bss last, the image ends at __el0_end and .bss is simply RAM beyond
     * it, which `_start` zeroes anyway. The stack sits above .bss and needs no
     * initialisation at all. */
    . = ALIGN(16);
    __bss_start = .;
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(16);
    __bss_end = .;

    /* 512 KiB boot stack, top-aligned. AAPCS64 requires SP 16-byte aligned; the
     * ALIGN(16) above plus a multiple-of-16 size keeps __stack_top aligned. */
    . = ALIGN(16);
    . += 0x80000;
    __stack_top = .;

    /* End of everything the kernel occupies. `memmap` carves this range out of
     * usable RAM, so it must sit after the LAST thing the kernel owns - the boot
     * stack included, or the allocator would be handed the stack it is standing
     * on. Note this is deliberately BEYOND the end of the image file: the image
     * stops at __el0_end, but the kernel still owns the .bss and stack above it. */
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.ARM.attributes) }
}
