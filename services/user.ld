/* services/user.ld - linker script for GodspeedOS ring-3 services.
 *
 * All userspace services load at 0x400000 (traditional ELF base).
 * Each service gets its own page table (separate CR3), so multiple services
 * can all use the same virtual base without conflict.
 *
 * Sections are page-aligned so the ELF loader can map each one with the
 * appropriate flags (text: rx, rodata: r, data/bss: rw).
 */

ENTRY(service_main)

SECTIONS {
    . = 0x400000;

    .text : {
        *(.text .text.*)
    }
    . = ALIGN(4096);

    .rodata : {
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
    }
    . = ALIGN(4096);

    .data : {
        *(.data .data.*)
        *(.got .got.*)
        *(.got.plt)
    }
    . = ALIGN(4096);

    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4096);

    _end = .;

    /* Discard unwinding tables - services use panic = "abort". On ARM, `.ARM.exidx`/`.ARM.extab` are
     * the unwind tables, and as orphan sections they land right after `.text` at a non-page-aligned
     * address, forming their own PT_LOAD segment that shares a page with the next one - which the
     * 4 KiB page-granular loader rejects. Discarding them (dead weight under panic=abort) keeps every
     * PT_LOAD page-aligned. No-op on x86, which has none of these. */
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.GNU-stack)
        *(.ARM.exidx*)
        *(.ARM.extab*)
        *(.ARM.attributes)
    }
}
