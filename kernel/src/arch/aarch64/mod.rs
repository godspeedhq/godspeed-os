// SPDX-License-Identifier: GPL-2.0-only
// AArch64 arch layer - NOT YET IMPLEMENTED. Design + plan: docs/aarch64.md.
//
// This stub exists so a `--target aarch64-unknown-none` build fails with a CLEAR message pointing at
// the plan, instead of a cryptic "file not found for module `aarch64`". It is `#[cfg(target_arch =
// "aarch64")]`-gated in arch/mod.rs, so it is never compiled on the x86_64 build (the compile_error!
// below cannot fire there).
//
// Implementing the port means replacing this file with a real module that exposes the SAME `arch::imp`
// surface `arch/x86_64/` does (docs/aarch64.md §1.1) - so the 126 arch-neutral call sites and the
// primitives they reach are a drop-in, touching zero neutral files. The surface, in dependency order:
//   - boot lifecycle: init, ap_init, ap_count, halt_all_cores, hardware_reset, BootInfo, switch_to_boot_stack
//   - MMU:            read_page_table_base, write_page_table_base, invalidate_tlb_page, page_tables::*  (TTBR0/1, TLBI)
//   - exceptions:     syscall_entry::* + EL0/EL1 fault discrimination (VBAR_EL1, SVC) - the C1/C2/K3 twin
//   - CPU + timer:    read_cycle_counter, init_timer + generic-timer compare (CNTPCT/CNTFRQ)
//   - IRQ + IPI:      disable/enable_interrupts, local_irq_save/restore, send_ipi_to_lapic,
//                     broadcast_ipi_all_but_self  (DAIF; GIC-400 distributor/CPU-interface + SGIs)
//   - serial:         serial_write_byte + the byte-in path (PL011)
//   - board (Pi 4):   pci (BCM2711 PCIe), rtc (none - no battery RTC), fb (VideoCore mailbox), iommu (no SMMU -> no-op)

compile_error!(
    "the AArch64 arch layer is not yet implemented - see docs/aarch64.md. Implement \
     kernel/src/arch/aarch64/ to the same `arch::imp` surface as arch/x86_64/ (Phase 0 is done: the \
     seam, the neutral-layer asm isolation, and the boundary guard are in place)."
);
