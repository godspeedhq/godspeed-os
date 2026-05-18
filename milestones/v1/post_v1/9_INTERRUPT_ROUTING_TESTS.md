# Post-v1 Item 9 — Interrupt Routing Tests

**Status:** ✅ Complete (identity tests IR1A/IR1B — 2026-05-18)

---

## Goal

Verify the §12 interrupt routing path with executable tests. The implementation
(post-v1 item 8) wired `deliver()`, `enqueue_from_interrupt`, `send_eoi`, and
`hw_irqs` at spawn. These tests confirm the path works end-to-end.

---

## Spec reference

**§12.2 Routing** — `deliver(irq)` must enqueue an interrupt-event message to
the registered driver endpoint; discard silently when no driver is registered;
EOI unconditionally.

**§12.3 Driver Capabilities** — a driver service that declares `hw_interrupt =
[N]` in its contract must receive interrupt events on its recv endpoint.

---

## IRQ injection mechanism

QEMU does not expose a per-IRQ software-fire command, and IDT software interrupts
(`int N`) require kernel ring-0 privilege. The test harness injects IRQs through
the existing COM2 control channel:

```
FIRE_IRQ <N>\n
```

`kernel/src/control.rs` dispatches this to `arch::x86_64::interrupts::fire_test_irq(N)`,
which:

1. Disables interrupts (satisfying `deliver`'s IF=0 requirement).
2. Calls `interrupt::route::deliver(N)` — enqueues the event, wakes the receiver,
   EOIs the APIC.
3. Re-enables interrupts.

`fire_test_irq` is a safe function in the permitted arch layer. It adds one
unsafe block to `arch/x86_64/interrupts.rs` (count 9→10); no grandfathered file
is touched.

---

## Identity tests (IR1A / IR1B) — §12.2, §12.3

These are the two tests added in this milestone. They are part of the core
`TESTS` array and run as part of `osdev test identity`.

### IR1A — Positive: driver receives interrupt event

```
test irq_delivery_driver_receives:
    probe-11a has hw_irqs = [33] → kernel calls register(33, probe-11a.endpoint)
    probe-11a: log "probe: 11A ready"; block on recv()

    harness: wait_for "probe: 11A ready"
    harness: send FIRE_IRQ 33 via COM2
    control channel: call fire_test_irq(33) → deliver(33) → enqueue to probe-11a

    probe-11a wakes: msg.payload[0] == 33
    probe-11a: log "probe: 11A pass irq=33"

    assert serial_contains("probe: 11A pass irq=33")
    assert kernel_did_not_panic()
```

### IR1B — Negative: unregistered IRQ is discarded, no panic

```
test irq_unregistered_discard_no_panic:
    harness: wait_for "supervisor: ready"
    harness: send FIRE_IRQ 34 via COM2   ← no driver registered for IRQ 34
    control channel: echo "control: FIRE_IRQ 34" on serial
    deliver(34): finds no endpoint → discard, EOI

    assert serial_contains("control: FIRE_IRQ 34")
    assert kernel_did_not_panic()
    assert no "probe: 11A FAIL"
```

---

## Acceptance criteria

- ✅ `fire_test_irq(N)` added to `arch/x86_64/interrupts.rs` (permitted layer,
      count 9→10); `FIRE_IRQ N` command added to `control.rs`.
- ✅ `probe-11a` ServiceConfig entry with `hw_irqs: &[33]`; spawned in identity
      build (both full and identity-only supervisor builds).
- ✅ Probe mode 160 (`MODE_IRQ_RECV`) added to probe service: logs "probe: 11A
      ready", blocks on recv, logs "probe: 11A pass irq=33" on receipt.
- ✅ Tests IR1A and IR1B added to `TESTS` in `osdev/src/validator.rs`.
- ✅ Unsafe audit updated (`interrupts.rs` 9→10, totals updated).
- ✅ All 20 existing identity tests still pass after the changes.

---

## Gap — property / fuzz / stress / adversarial tests

The following test categories are NOT implemented in this milestone:

| Gap | Description |
|-----|-------------|
| Property (PIR1) | IRQ number round-trips faithfully: `deliver(N)` → `recv()` → `payload[0] == N` for all 256 values |
| Property (PIR2) | Full queue on interrupt delivery: deliver discards instead of blocking, EOI still fires |
| Fuzz (FIR1) | All 256 IRQ values via FIRE_IRQ; none may panic |
| Fuzz (FIR2) | Rapid FIRE_IRQ on the same endpoint: 1000 deliveries; no deadlock, no queue overflow panic |
| Stress (SIR1) | Sustained FIRE_IRQ for 1 minute; probe-11a drains at half the delivery rate; no panic, no stuck IRQ line |
| Adversarial (AIR1) | Service without `hw_interrupt` cap cannot register for IRQs; `deliver(N)` delivers only to the registered endpoint |

These will be addressed in a follow-on milestone once the identity tests
confirm the basic path is sound.

---

## Implementation (2026-05-18)

### Files changed

| File | Change |
|------|--------|
| `kernel/src/arch/x86_64/interrupts.rs` | Added `fire_test_irq(irq: u8)` (unsafe count 9→10) |
| `kernel/src/control.rs` | Added `FIRE_IRQ N` command handler |
| `kernel/src/task/mod.rs` | Added `probe-11a` ServiceConfig with `hw_irqs: &[33]`, `probe_mode: 160` |
| `services/probe/src/main.rs` | Added `MODE_IRQ_RECV = 160` and `mode_irq_recv` handler |
| `services/supervisor/src/main.rs` | Added `spawn("probe-11a")` in identity-build spawn list |
| `osdev/src/validator.rs` | Added IR1A, IR1B to `TESTS` |
| `docs/unsafe-audit.md` | Updated `interrupts.rs` count 9→10, permitted total 216→217 |
| `tests/qemu/identity/CLAUDE.md` | Added IR1A/IR1B to test table |

### Key design decisions

- **IRQ 33 for IR1A / IRQ 34 for IR1B**: IRQ 33 (0x21) is not used by any
  real hardware in the QEMU test environment; IRQ 34 is deliberately
  unregistered to prove silent discard.
- **fire_test_irq in arch layer**: Keeps `interrupt/route.rs` grandfathered
  count at 1. The arch layer is the permitted location for hardware-adjacent
  unsafe (§18.1); the IF=0 requirement makes this arch-level work.
- **WithRestart harness kind for both tests**: IR1A uses `wait_for`
  "probe: 11A ready" to guarantee the probe is alive before injection. IR1B
  uses `wait_for` "supervisor: ready" to guarantee a stable system before
  the unregistered-IRQ injection.

### Verification

22/22 identity tests pass after all changes (2026-05-18, Windows TCG).
(20 existing + IR1A + IR1B = 22 total in `TESTS`.)
