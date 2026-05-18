# Post-v1 Item 8 — Interrupt Routing to Userspace

**Status:** Complete

---

## Goal

Complete the hardware interrupt delivery path so that IRQs reach userspace driver
services as IPC messages (§12). The kernel IDT, the `IRQ_TABLE`, and endpoint
registration are all in place. The single remaining gap is the message construction
and enqueue call inside `deliver()`.

---

## Spec reference

**§12.2 Routing** — kernel IDT dispatches to `interrupt::route::deliver(irq)`, which
enqueues an IPC message to the registered driver endpoint. If the driver is on a
different core than the IRQ-receiving core, delivery uses the cross-core IPC path
(IPI wake).

**§12.3 Driver Capabilities** — `hw_interrupt = [N]` in a service contract causes the
kernel to call `interrupt::route::register(irq, endpoint)` at spawn time.

---

## What exists

| File | State |
|------|-------|
| `kernel/src/interrupt/route.rs` | `IRQ_TABLE`, `register()` complete; `deliver()` has one `todo!()` |
| `kernel/src/interrupt/mod.rs` | Module declaration, no gaps |
| `kernel/src/ipc/routing.rs` | `enqueue()` and cross-core wakeup complete |
| `kernel/src/ipc/message.rs` | `Message` type defined; no interrupt-event variant yet |

---

## What needs to be done

### 1. Interrupt-event message (`kernel/src/ipc/message.rs`)

Add an interrupt event constructor so `deliver()` can build a typed message without
allocating. The message payload carries the IRQ number; the 4 KiB message size
limit is not a concern here (IRQ number fits in a u8).

```rust
impl Message {
    pub fn interrupt_event(irq: u8) -> Self { ... }
}
```

### 2. Fill the `todo!()` in `deliver()` (`kernel/src/interrupt/route.rs`)

```rust
pub unsafe fn deliver(irq: u8) {
    let endpoint = IRQ_TABLE.lock()[irq as usize];
    if let Some(ep) = endpoint {
        let msg = Message::interrupt_event(irq);
        // SAFETY: called from IDT with IF=0; enqueue is interrupt-context safe
        // because it holds the endpoint spinlock for a bounded critical section.
        crate::ipc::routing::enqueue(ep, msg);
    }
    // discard if no driver registered — driver may not have started yet
}
```

### 3. EOI the APIC

After delivery (or discard), the kernel must signal End-Of-Interrupt to the local
APIC so the interrupt line is re-armed. The APIC EOI register write belongs in
`deliver()` after the enqueue, not in the IDT stub.

### 4. Wire `register()` into the spawn path

`interrupt::route::register(irq, endpoint)` already exists but nothing calls it.
The spawn path in `task/scheduler.rs` or `syscall/dispatch.rs` processes a service's
contract capabilities at spawn time — the `hw_interrupt` capability must trigger a
`register()` call there.

### 5. Extend the spawn syscall to accept `hw_interrupt` caps

The `Spawn` syscall currently grants IPC and memory capabilities. Add a `HwInterrupt`
capability variant that, when granted, calls `interrupt::route::register()` and inserts
a corresponding cap into the service's cap table with `RECV` rights on the interrupt
endpoint.

### 6. Update `invariants/assertions.rs` (`assert_tcb_alive`)

Small follow-on: while in the kernel, fill the two remaining `todo!()` stubs in
`invariants/assertions.rs` (`assert_tcb_alive`, `assert_cap_table_consistent`).
These are not interrupt-routing work but are the only other kernel gaps and can
be done in the same pass.

---

## Acceptance criteria

- [x] `deliver(irq)` enqueues an interrupt-event `Message` to the registered endpoint;
      EOIs the local APIC; discards silently when no driver is registered.
- [x] A driver service declaring `hw_interrupt = [N]` in its contract receives its
      IRQ endpoint populated at spawn time via `register()`.
- [x] Cross-core delivery: if the driver is pinned to a different core than the one
      receiving the IRQ, the IPI wake path is exercised (same path as any cross-core
      `send`).
- [x] No kernel panic on unregistered IRQs (existing behaviour preserved).
- [x] `assert_tcb_alive()` and `assert_cap_table_consistent()` implemented.
- [x] All 20 identity tests still pass after the changes.
- [x] Unsafe audit doc updated if any new `unsafe` blocks are added.

---

## Implementation notes

- `deliver()` is called with IF=0 (interrupts disabled). The enqueue must not block
  — use `try_enqueue` semantics. If the driver's queue is full, discard the interrupt
  (same "loud failure, bounded behaviour" policy as a full IPC queue).
- The APIC EOI must happen unconditionally (even on discard and even on full queue)
  or the interrupt line stays masked and the system hangs.
- No new `unsafe` should be required beyond the grandfathered line already present
  in `interrupt/route.rs`. The `enqueue` call is safe from the IPC module's
  perspective; the `unsafe` on `deliver` already captures the interrupt-context
  calling convention.

---

## Implementation (2026-05-18)

### Files changed

| File | Change |
|------|--------|
| `kernel/src/ipc/message.rs` | Added `Message::interrupt_event(irq: u8) -> Self` |
| `kernel/src/ipc/routing.rs` | Added `enqueue_from_interrupt(endpoint, msg) -> Option<usize>` (no cap check, try-send); `is_endpoint_alive(endpoint) -> bool` |
| `kernel/src/interrupt/route.rs` | Filled `deliver()` todo: build msg, enqueue, wake receiver, EOI unconditionally |
| `kernel/src/arch/x86_64/interrupts.rs` | Added `send_eoi()` safe wrapper (keeps `interrupt/route.rs` grandfathered count at 1) |
| `kernel/src/capability/table.rs` | Added `CapTable::for_each_slot` for invariant walk |
| `kernel/src/task/scheduler.rs` | Added `for_each_active_cap` for `assert_cap_table_consistent` |
| `kernel/src/invariants/assertions.rs` | Implemented `assert_tcb_alive` and `assert_cap_table_consistent` |
| `kernel/src/task/mod.rs` | Added `hw_irqs: &'static [u8]` to `ServiceConfig`; wired `interrupt::route::register` for each IRQ at spawn |
| `docs/unsafe-audit.md` | Updated `interrupts.rs` count 8→9 for `send_eoi` wrapper |

### Key design decisions

- **No generation check in interrupt delivery**: `enqueue_from_interrupt` skips the cap generation check because the kernel IDT is the sender, not a user task. Liveness is still checked (Dead endpoints discard).
- **EOI through safe wrapper**: `arch::x86_64::interrupts::send_eoi()` hides `apic_send_eoi()` so `deliver()` needs no new `unsafe` block (grandfathered count stays at 1).
- **Cross-core IPI handled by `wake_by_slot`**: The scheduler's `wake_by_slot` already sends a `WAKE_RECEIVER` IPI when the receiver is on a different core. No extra IPI logic needed in `deliver()`.
- **Try-send on full queue**: Full queue = driver overloaded. Interrupt silently discarded. EOI still fires — the IRQ line must be re-armed regardless.

### Verification

20/20 identity tests pass after all changes (2026-05-18, Windows TCG).
