ENTRY(_start)

/* Explicit program headers so lld never splits .got and .requests into
   separate PT_LOAD segments.  When they share a page but live in different
   PT_LOADs, Limine's ELF loader maps page 0 for the first segment and then
   tries to map the same page again for the second — it silently stops
   processing subsequent segments, so .rodata and .bss are never mapped. */
PHDRS {
    requests PT_LOAD FLAGS(6) ;   /* rw-  GOT + Limine request structs      */
    text     PT_LOAD FLAGS(5) ;   /* r-x  kernel code                       */
    rodata   PT_LOAD FLAGS(4) ;   /* r--  read-only data (GDT, strings, …)  */
    data     PT_LOAD FLAGS(6) ;   /* rw-  .data + .bss                      */
}

SECTIONS {
    /* Limine protocol v3+ requires kernel virtual addresses in the higher half
       (vaddr >= 0x8000000000000000).
       0xFFFFFFFF80100000 = upper-2GB + 1 MiB: standard higher-half base.    */
    . = 0xFFFFFFFF80100000;

    /* Pull .got into the requests segment so it shares one PT_LOAD with
       .requests instead of getting its own segment header. */
    .got : { *(.got .got.*) } :requests

    /* Limine request structs — KEEP so the linker never discards them.
       RequestsStartMarker and RequestsEndMarker bracket the requests region. */
    .requests : {
        KEEP(*(.requests_start))
        KEEP(*(.requests .requests.*))
        KEEP(*(.requests_end))
    } :requests

    .text : ALIGN(0x1000) {
        KEEP(*(.text._start))
        *(.text .text.*)
    } :text

    .rodata : ALIGN(0x1000) {
        *(.rodata .rodata.*)
        *(.eh_frame_hdr)
    } :rodata

    .data : ALIGN(0x1000) {
        *(.data .data.*)
    } :data

    .bss : ALIGN(0x1000) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
        *(.comment)
    }
}
