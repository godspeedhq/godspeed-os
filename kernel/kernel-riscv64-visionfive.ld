/* RISC-V linker script for the StarFive VisionFive 2 Lite (JH7110), S-mode via OpenSBI.
 *
 * SAME AS `kernel-riscv64.ld` EXCEPT THE LOAD ADDRESS, and that one number is the whole reason this
 * file exists. QEMU `virt` puts RAM at 0x8000_0000 and OpenSBI enters the kernel at 0x8020_0000;
 * this board puts RAM at 0x4000_0000 and enters at 0x4020_0000. Both numbers were read off the
 * board itself rather than from documentation - OpenSBI prints `Domain0 Next Address : 0x40200000`,
 * and the vendor image's own `uEnv_Lite.txt` sets `kernel_addr_r=0x40200000`. Two independent
 * sources agreeing is why this is a constant and not a guess.
 *
 * The UART does NOT change: the JH7110 puts its 16550A at 0x1000_0000, which is where QEMU puts its
 * NS16550 too. That is luck, not design, and it is the reason the same banner code prints on both. */

ENTRY(_start)

SECTIONS {
    . = 0x40200000;

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
