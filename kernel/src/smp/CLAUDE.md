# kernel/src/smp/

Multi-core coordination (§9, §11). Unsafe boundary: APIC MMIO writes in `ipi.rs`.

## Files

| File            | Responsibility |
|-----------------|---------------|
| `mod.rs`        | `init(boot_info)`: init per-core state, start APs |
| `core.rs`       | `CoreState` per core; `mark_ready(core_id)`, `ready_count()`, `is_ready(core_id)` |
| `ipi.rs`        | `send_ipi(core_id, vector)`, `broadcast_tlb_shootdown(virt)`, `ipi_handler(vector)` |
| `placement.rs`  | `resolve(contract_core)` → `Ok(core_id)` or `Err(PlacementInvalid)` |

## Core lifecycle (§9.5)

Cores discovered at boot are fixed for the system lifetime. No hotplug. The core count is `BootInfo.ap_ids.len() + 1` (the +1 is the BSP). Any core that fails to call `mark_ready` within the timeout is logged as a warning and excluded from placement (§11.3).

## IPI vectors

Three distinct vectors, defined in `ipi::vectors`:
- `WAKE_RECEIVER (0xF0)` — wake a task blocked on `recv` (used by cross-core `send`).
- `TLB_SHOOTDOWN (0xF1)` — invalidate a page on remote TLBs (used on unmap).
- `SCHEDULER_TICK (0xF2)` — force a scheduling point (used by timer overflow broadcast).

## TLB shootdown protocol (§10.5)

1. Caller disables interrupts on its core.
2. Calls `broadcast_tlb_shootdown(virt_addr)`.
3. Each receiving core's `ipi_handler` calls `invlpg(virt_addr)` and increments the ack counter.
4. Caller spins until `ack_count == ready_count - 1`.
5. Caller re-enables interrupts and proceeds with frame reclaim.

This is a synchronous barrier. It is a real cost on every unmap. v1 minimises unmap frequency by reclaiming memory only at service death (§10.5).

## Placement (§9.2)

`resolve(contract_core)` returns the core a new service instance should run on:
- `Some(n)` → requires core `n`; returns `PlacementInvalid` if `!is_ready(n)`.
- `None` → round-robin via `RR_COUNTER % ready_count`.

On restart, `resolve` is called again with the same contract. The previous core is not remembered (§9.2 "on restart" clause).
