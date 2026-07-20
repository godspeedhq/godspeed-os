/* ARMv7-A linker script for the Raspberry Pi 2 Model B (BCM2836, Cortex-A7).
 *
 * The Pi firmware loads `kernel7.img` (the `7` means ARMv7 - it selects the image
 * BY NAME) as a FLAT BINARY at 0x8000 and branches to the first byte, so `_start`
 * must be physically first in the image. `.text.boot` + KEEP + ENTRY pin it there.
 *
 * No higher-half: the MMU is off at boot, so we run identity-mapped at this low
 * physical address until page tables exist. 948 MiB is usable below the VideoCore
 * split (Linux on this board reports mem 0x0-0x3b3fffff), so the stack placed
 * just past BSS is comfortably inside RAM.
 */

ENTRY(_start)

SECTIONS {
    . = 0x8000;

    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))     /* _start first - the firmware branches to byte 0 */
        . = ALIGN(32);          /* VBAR ignores the low 5 bits: the table MUST be 32-byte aligned */
        KEEP(*(.text.vectors))
        *(.text .text.*)
    }

    .rodata : ALIGN(0x1000) { *(.rodata .rodata.*) }
    .data   : ALIGN(0x1000) { *(.data .data.*) }

    . = ALIGN(8);
    __bss_start = .;
    .bss : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(8);
    __bss_end = .;

    /* 512 KiB boot stack (SVC mode), top-aligned. AAPCS wants SP 8-byte aligned;
     * the ALIGN(8) above plus a multiple-of-8 size keeps __stack_top aligned. */
    . = ALIGN(8);
    . += 0x80000;
    __stack_top = .;

    /* One 4 KiB stack per exception mode. ARMv7 banks SP per mode, and at reset
     * those banked registers hold garbage - a handler that pushes before its
     * stack exists faults INSIDE the fault, which is an unrecoverable silent
     * loop. 4 KiB each is ample for report-and-halt and keeps the bound visible
     * (§26.6.1). Grow these when handlers start saving full context. */
    . = ALIGN(8);
    . += 0x1000;
    __abt_stack_top = .;
    . += 0x1000;
    __und_stack_top = .;
    . += 0x1000;
    __irq_stack_top = .;
    . += 0x1000;
    __fiq_stack_top = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.eh_frame_hdr) *(.ARM.exidx*) *(.ARM.attributes) }
}
