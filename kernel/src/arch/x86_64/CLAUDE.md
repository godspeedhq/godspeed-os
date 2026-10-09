# kernel/src/arch/x86_64/

The unsafe hardware boundary (§18.1). All direct hardware access in the kernel lives here.

## Files

| File                | Responsibility |
|---------------------|---------------|
| `mod.rs`            | Public API surface for the rest of the kernel (the `arch::imp` seam): the Limine requests and `_start`, `BootInfo`, `init()`, `ap_init()`, the COM1/COM2 serial paths and console input ring, `halt_all_cores()`, `hardware_reset()` |
| `boot.rs`           | BSP init: GDT, IDT (including the exception and page-fault handlers), local APIC and timer calibration, SYSCALL MSRs (§11.1) |
| `ap_boot.rs`        | AP startup: Limine's `goto_address` per AP, then per-AP init (§11.2) |
| `interrupts.rs`     | IRQ and MSI dispatch stubs, `wait_for_interrupt` and the idle-halt decision, EOI (§12) |
| `syscall_entry.rs`  | SYSCALL entry stub, per-core syscall data, user-pointer validation and copies (§8.2) |
| `ioapic.rs`         | I/O APIC: routing legacy INTx device interrupts to a local APIC (§12) |
| `pci.rs`            | PCI configuration access and enumeration (§12) |
| `iommu.rs`          | AMD-Vi IOMMU: detection from IVRS, per-device DMA confinement, the event log (§6.4) |
| `rtc.rs`            | MC146818 CMOS real-time clock |
| `context_switch.rs` | Naked function: save/restore callee-saved registers + CR3 (§9) |
| `page_tables.rs`    | Four-level page table manipulation: map/unmap, CR3 values (§10) |
| `fb.rs`             | Boot/panic-console **backend** only: binds Limine's descriptor to a slice for `crate::bootcon`, recovers its PHYSICAL base for the `console` service's framebuffer grant, and publishes writes (`fb_commit` = `sfence`, because the mapping is write-combining). The terminal itself is the userspace `console` service - see `kernel/CLAUDE.md` and `docs/console-service.md` 9 |

## Safe wrappers (call these instead of writing new unsafe blocks)

These functions in `arch::x86_64` expose hardware operations as a safe API. If you need one of these operations outside the arch layer, use the wrapper - do not write a new `unsafe` block.

| Function                     | What it wraps |
|------------------------------|---------------|
| `disable_interrupts()`       | `cli` |
| `enable_interrupts()`        | `sti` |
| `wait_for_interrupt()`       | **Two behaviours, chosen at boot by `IDLE_CAN_HALT`:** `sti; hlt` where a halted core is guaranteed to wake (AMD; Intel with the package C-state limit applied; or ARAT in periodic mode), else `sti` only - no C-state hint, because on Goldmont+ both `hlt` and `pause` let firmware power-gate the LAPIC and drop ticks/IPIs. **A caller must arm a wake before it halts** - see below. |
| `validate_user_ptr(ptr, len)`| Range check: ptr..ptr+len must be below `USER_END` (0x0000_8000_0000_0000) |
| `read_user_bytes(ptr, len)`  | Validated `from_raw_parts` into user VA |
| `write_user_bytes(dst, src)` | Validated `copy_nonoverlapping` to user VA |
| `read_cycle_counter()`       | `RDTSC` |
| `com2_init()`                | COM2 UART init (control channel for test harness) |

## The idle contract: never halt without a freshly armed wake

In **TSC-Deadline** mode the LAPIC timer is **one-shot** - software re-arms it on every tick. A core that
halts is therefore relying on a deadline already in flight, and if that deadline has been consumed the
core never wakes again. The scheduler's idle path must arm one first:

- an **AP** arms the long idle deadline (`rearm_idle_timer`, ~1 s - deliberately under the liveness
  watchdog's ~3 s threshold so an idle core still stamps `CORE_LAST_TICK_TSC` and reads as alive);
- the **BSP** arms the normal quantum (`rearm_quantum_timer`), because it must keep driving
  `MONOTONIC_TICKS`, `scan_timed_wakes` and the COM polling at ~100 Hz and so cannot slow down.

This was found the hard way (2026-07-31): the BSP was excluded from the idle re-arm and then allowed to
`hlt` anyway, so on the AMD T630 it halted onto a consumed deadline and the liveness watchdog panicked
with `core 0 made NO progress ... slot 224` (224 = `IDLE`) ~5 s after boot. It had been latent for the
life of the port, invisible only because userspace spin-yielded and the BSP never actually reached idle.

**Periodic mode needs no fresh deadline** - the hardware auto-reloads, so a halted core keeps being
woken. It has the opposite trap instead: every write of the initial count RESTARTS the countdown, so
an idle path that rewrites it on each pass can keep a frequently-woken core from ever taking a tick
(the T630, 2026-10-08). `boot.rs`'s `TIMER_MODE` records which period a core is counting so neither
re-arm restarts a countdown it does not have to. (Which machine runs which mode is printed at boot:
QEMU and the T630 run periodic, the Wyse 5070 TSC-Deadline.)

## Boot protocol: Limine

The kernel uses the Limine Boot Protocol (`limine` crate). Request structures are declared as Rust statics in `mod.rs`; Limine fills them in before jumping to `_start`, which builds `BootInfo` and calls `kernel_main`. Requests consumed:
- `MemmapRequest` - physical memory layout
- `HhdmRequest` - higher-half direct map base address
- `MpRequest` - AP LAPIC IDs, and the per-AP `bootstrap` that starts them (eliminates need for ACPI/MADT parsing)
- `FramebufferRequest` - early output
- `ExecutableAddressRequest` - physical/virtual base of the kernel image, so the frame allocator can exclude it
- `RsdpRequest` - the ACPI RSDP, from which `iommu.rs` finds the IVRS table

## Invariants

- `init()` is called exactly once, by the BSP, before any other kernel subsystem.
- `ap_init(core_id)` is called exactly once per AP, from `ap_main`.
- Every `unsafe` block carries a SAFETY comment. Many hardware operations are exposed as safe `fn`s wrapping one (`com2_init`, `serial_write_byte`); a precondition that is only boot ordering is a documented contract, not an `unsafe fn` (§18.5).
- COM1 is written only by `serial_write_byte` and `serial_write_bytes_lockfree` (and the `_nolck` fault writers); `log.rs` calls `serial_write_bytes_lockfree`.
- COM2 is the operator control channel, read by the userspace `control` service through `com2_try_read_byte` (there is no `control.rs` in the kernel); the arch layer only configures and probes it (`com2_init`).

## Context switch contract

`switch_context(current, next)` is a naked function. The caller (scheduler) must:
1. Disable interrupts before calling.
2. Re-enable interrupts after the switch if the incoming task expects them enabled.
3. Never call it with the same pointer for both arguments.

## Page table contract

`PageTable::unmap` returns the physical frame but does NOT issue a TLB shootdown; a caller that unmaps a page another core may have cached must flush it (§10.5). Today only selftests call it. Task death does not unmap page by page: the kill path (`task/scheduler.rs`) waits until every other core has loaded a different CR3, relies on that reload having flushed the task's non-global entries, and deliberately issues NO `smp::ipi::broadcast_tlb_shootdown` (which has no caller). That reliance is an x86 semantic - see `arch/CLAUDE.md`, SEC-26. See also the PML4 deferred-free note in `kernel/src/task/CLAUDE.md`.
