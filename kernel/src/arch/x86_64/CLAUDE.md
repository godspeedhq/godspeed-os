# kernel/src/arch/x86_64/

The unsafe hardware boundary (§18.1). All direct hardware access in the kernel lives here.

## Files

| File                | Responsibility |
|---------------------|---------------|
| `mod.rs`            | Public API surface for the rest of the kernel; re-exports `BootInfo`, `init()`, `ap_init()`, `serial_write_byte()`, `halt_all_cores()` |
| `boot.rs`           | BSP init: GDT, IDT, paging, local APIC (§11.1 step 1) |
| `ap_boot.rs`        | AP startup: real-mode trampoline, INIT+SIPI sequence (§11.2) |
| `interrupts.rs`     | IDT entries, IRQ dispatch stubs, page-fault handler (§12, §10.3) |
| `context_switch.rs` | Naked function: save/restore callee-saved registers + CR3 (§9) |
| `page_tables.rs`    | Four-level page table manipulation: map/unmap, CR3 values (§10) |

## Safe wrappers (call these instead of writing new unsafe blocks)

These functions in `arch::x86_64` expose hardware operations as a safe API. If you need one of these operations outside the arch layer, use the wrapper — do not write a new `unsafe` block.

| Function | What it wraps |
|---|---|
| `disable_interrupts()` | `cli` |
| `enable_interrupts()` | `sti` |
| `wait_for_interrupt()` | `sti; hlt` (atomic enable + halt for idle loop) |
| `validate_user_ptr(ptr, len)` | Range check: ptr..ptr+len must be below `USER_END` (0x0000_8000_0000_0000) |
| `read_user_bytes(ptr, len)` | Validated `from_raw_parts` into user VA |
| `write_user_bytes(dst, src)` | Validated `copy_nonoverlapping` to user VA |
| `read_cycle_counter()` | `RDTSC` |
| `com2_init()` | COM2 UART init (safe — inner `outb` stays unsafe in arch) |

## Invariants

- `init()` is called exactly once, by the BSP, before any other kernel subsystem.
- `ap_init(core_id)` is called exactly once per AP, from `ap_main`.
- Every function in this module that touches hardware is `unsafe` with a SAFETY comment.
- `serial_write_byte` is the only path that writes to COM1; the ring buffer in `log.rs` calls it.

## Context switch contract

`switch_context(current, next)` is a naked function. The caller (scheduler) must:
1. Disable interrupts before calling.
2. Re-enable interrupts after the switch if the incoming task expects them enabled.
3. Never call it with the same pointer for both arguments.

## Page table contract

`PageTable::unmap` returns the physical frame but does NOT issue a TLB shootdown. The caller (`memory::ownership` on task death) must call `smp::ipi::broadcast_tlb_shootdown` before returning the frame to the allocator (§10.5).
