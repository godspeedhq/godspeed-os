# Bug 1 — Cross-core IPC reply to the BSP (core 0) never reaches a blocked receiver

**Status:** Open · confirmed real (reproduces on a clean, diagnostic-free kernel)
**Severity:** High — blocks the cross-core IPC round-trip (BP2 benchmark / any request→reply where the requester is on the BSP)
**Hardware:** HP T630 thin client — AMD GX-420GI (Jaguar/Puma+, Family 22h), 4 cores ~2 GHz, 8 GB RAM. LAPIC IDs 16/17/18/19 (BSP = LAPIC 16 = logical core 0).
**First seen:** 2026-05-29 · last reproduced clean: 2026-05-30
**Affected branch/commit:** reproduces on `main` @ `059b104` (the ring-3 AMD userspace milestone). Debug instrumentation lives on branch `debug/bug-a-bsp-timer`.

---

## One-line summary

A task that performs a blocking `recv` **on core 0 (the BSP)** is never delivered a
message that another core (`AP`) enqueues for it — the receiver stays starved/stalled.
Traffic in the other direction (BSP → AP) works perfectly. This is a software bug, **not**
an APIC/timer hardware limitation (proven below).

---

## Reproduction

```
osdev image --mode bp2-only      # sender (perf-bp2) on core 0, echo (perf-bp2-echo) on core 1
# flash build/os.img to USB, boot the T630, capture COM1 @ 115200 8N1
```

The BP2 probe does a cross-core request→reply round-trip:
- `perf-bp2` (sender, **core 0**): `send → echo`, then `recv` its reply on its own endpoint `perf-bp2`.
- `perf-bp2-echo` (echo, **core 1**): `recv`, then `try_send("perf-bp2", reply)` in a retry loop.

Observed serial (clean kernel, no diagnostics):

```
init: ready
supervisor: ready
pong: ready on core 1
ping: starting
perf: BP2 sender start
perf: BP2 sender sent-0      <- sender's send to echo succeeded (core0 -> core1)
perf: BP2 echo recv-0        <- echo received it (core0 -> core1 wake WORKS)
perf: BP2 echo sent-0        <- echo's try_send("perf-bp2") returned Ok => reply was ENQUEUED (core1 -> core0)
‹stall›                      <- sender NEVER logs "recv-0 OK"; round-trip 0 never completes
pong: received "42905"       <- meanwhile ping(core0) -> pong(core1) one-way runs fine, climbing
```

`recv-0 OK` count = 0, `BP2 done` = 0, highest sender iteration = 0.

---

## Precise symptom

- Direction **core 0 → core 1** works: `sender → echo` delivered+woke echo; `ping → pong`
  runs to 40k+. The BSP can wake a blocked receiver on an AP.
- Direction **core 1 → core 0** fails: echo's reply is accepted by `try_send` (returns `Ok`,
  so it was enqueued / handed to the blocked-receiver path), but the sender blocked on
  `recv` on core 0 **never receives it** and never resumes.
- Under heavy ambient churn (full `perf-brutal`, many cross-core IPIs), the round-trip
  occasionally completes — **4/1000** in one run — but **0/1000** in isolation (`bp2-only`).
  Racy, not a hard failure.

## What runs on which core (bp2-only)

```
core 0: init(0) supervisor(1) registry(2) logger(3) ping(5) perf-bp2/sender(6)
core 1: pong(4) perf-bp2-echo(7)
```
ping (core 0) calls `yield_cpu()` every loop iteration (~40k+ yields observed), so core 0's
`pick_next` runs tens of thousands of times — yet the Ready sender (slot 6) is not serviced
to completion.

---

## Key proven facts (hypotheses RULED OUT)

1. **NOT an APIC timer hardware/delivery failure on the BSP.**
   A deterministic boot-time probe (`irrwatch`, each core spins IF=0 until 2 timer reloads,
   watching `TIMER_CURRENT` and `IRR[0x20]`) reported **identical** results on all four cores
   incl. the BSP:
   ```
   irrwatch: core 16 (BSP) irr_ever=1 reloads=2 mincur=1
   irrwatch: core 17/18/19  irr_ever=1 reloads=2 mincur=1
   ```
   i.e. the BSP timer counts to 0, reloads, and **raises the interrupt into IRR**, just like
   the APs. The LVT timer reads `lvt=0x20020` (periodic, vector 0x20, **unmasked**), `TPR=0`,
   `init=6_250_000`, counter decrements. The timer hardware is fine.

2. **NOT a periodic-vs-TSC-deadline / mask / TPR issue.** `apicchk` confirmed `lvt=0x20020`,
   `TPR=00`, counter counting on all cores.

3. **NOT the LAPIC-id → logical-core mapping.** `lapic_to_core_id(16)` resolves the BSP to
   logical 0 (via the `Returns 0 if no match` fallback in `smp/core.rs`); cores run with
   correct ids.

4. **NOT my diagnostics causing the BP2 stall.** The clean `main` kernel (zero diagnostics)
   reproduces the stall exactly. (However the diagnostics DID confound other runs — see below.)

5. **NOT a `SpinLock<T>` deadlock.** A stuck-lock detector (prints `LOCKSTUCK <addr>` after
   80M spins inside `SpinLock::lock`) never fired during the stall.

---

## Current best understanding (unproven)

At runtime, **core 0 services no interrupts** while stalled (`c0=0`: `timer_tick_from_irq`
ran 0 times on logical core 0 across many runs, while core 1 logged dozens). Since the timer
provably *fires* (fact 1), the only explanation is that **core 0 runs with `IF=0` (interrupts
disabled) for long/indefinite stretches**, so the pending timer (and any `WAKE_RECEIVER` IPI)
is never taken — hence no preemption and no cross-core wake servicing on the BSP.

Two distinct manifestations were seen, run-to-run (it's a race):
- **(common)** ping runs on core 0 (40k+ sends, yielding each time) and the woken sender
  (slot 6) is `Ready` but `pick_next` never schedules it to completion. The reply was
  enqueued (echo `sent-0`), yet the sender's `recv` never returns it.
- **(occasional, only with serial diagnostics present)** the sender itself wedges `Running`
  on core 0 and never reaches `handle_recv`'s loop.

The tension not yet resolved: with ping yielding ~40k times, `pick_next(0)`'s round-robin scan
*should* return the Ready sender (slot 6, core 0) within a few picks, and `dequeue` *should*
find echo's enqueued reply. One of these is silently not happening on real SMP hardware. The
IPC/scheduler logic was audited statically and appears correct for every interleaving (routing
`TABLE` SpinLock serializes enqueue/dequeue + blocked-receiver recording; `block_and_reschedule`
↔ `wake_by_slot` CAS handshake closes the lost-wakeup window; `pick_next` hint + RR scan; the
`SpinLock` uses Acquire/Release). So the defect is either a real-SMP memory-ordering/race that
the static reading misses, or an `IF=0` wedge in a path not yet instrumented.

QEMU note: never reproduces under QEMU TCG (which serializes cores) — real-SMP only. This
matches prior `feedback-cas-not-store` findings.

---

## IMPORTANT confound discovered (affects how to instrument)

The serial path has a latent bug that **invalidates serial-heavy diagnostics**:
- `arch::x86_64::serial_write_byte` (the `kprintln` path) holds `SERIAL_LOCK` and then does an
  **unbounded** `while (inb(COM1+5) & 0x20) == 0 {}` THRE poll (no cap).
- `arch::x86_64::serial_write_bytes_lockfree` (used by ALL the debug probes) **bypasses
  `SERIAL_LOCK`** and does its own THRE poll + write.

So multiple cores writing COM1 (locked `kprintln` + lockfree probes) interleave and corrupt
the UART TX/THRE state; a THRE poll can then never complete → a core spins forever with `IF=0`
→ an *induced* wedge that is NOT the real bug. This is why a diagnostic build once wedged at
`init`'s very first `kprintln` ("init: ready"). **Future instrumentation must be memory-based
(atomic counters dumped rarely), not lockfree-serial.** This is also a genuine latent bug worth
fixing on its own (bound the THRE poll; make the lockfree writer respect `SERIAL_LOCK`).

---

## Code locations to examine

- `kernel/src/syscall/dispatch.rs` — `handle_recv` (block loop), `handle_send`/`handle_try_send`.
- `kernel/src/ipc/routing.rs` — `enqueue_locked` / `dequeue_locked` (blocked_receiver recording,
  the `TABLE` SpinLock), `endpoint_queue_depth`.
- `kernel/src/task/scheduler.rs` — `block_and_reschedule`, `wake_by_slot` (cross-core CAS-retry
  + `CORE_WAKE_HINT` + `send_ipi`), `pick_next` (hint fast-path + RR scan), `yield_current`,
  `timer_tick_from_irq`.
- `kernel/src/smp/ipi.rs` — `send_ipi` (ICR write + DELIVS poll), `WAKE_RECEIVER` (0xF0) path.
- `kernel/src/arch/x86_64/boot.rs` — `ipi_wake_stub` (IDT[0xF0] → `timer_tick_from_irq`),
  `init_local_apic` (TPR zero, LVT timer, periodic vs TSC-deadline), `apic_send_eoi`.
- `kernel/src/smp/core.rs` — `lapic_to_core_id`, `current_core_id` source.
- `kernel/src/arch/x86_64/mod.rs` — `serial_write_byte` (unbounded THRE), `serial_write_bytes_lockfree`
  (SERIAL_LOCK bypass) — the confound / latent bug.

---

## Open questions for research

1. On AMD Family 22h (Jaguar), are there known issues delivering a fixed-vector **IPI to the
   BSP**, or with the BSP servicing its periodic LVT timer, that differ from APs? (We proved
   the timer *fires into IRR*; the question is why the BSP appears to run `IF=0` so long that it
   never takes it.)
2. Is there a kernel path on the `recv`/reply side that disables interrupts (`cli`, interrupt-gate
   syscall entry, a spinlock acquired with IF=0) and can spin indefinitely on real SMP — i.e. a
   genuine deadlock/livelock distinct from the (ruled-out) `SpinLock<T>` and serial cases?
3. Does the cross-core `wake_by_slot` → `CORE_WAKE_HINT[0]` + `WAKE_RECEIVER` IPI to the BSP
   actually get serviced? (Needs a memory-counter probe: count `wake_by_slot(slot=6)` calls vs
   `pick_next` returning slot 6 on core 0 vs the sender's `dequeue` results.)
4. Could `pick_next`'s `CORE_WAKE_HINT` fast-path be perpetually short-circuiting the RR scan on
   core 0, starving slot 6? (The hint is only set on cross-core wakes; needs counting.)

---

## Suggested next diagnostic (non-confounding)

Memory-based only (no serial flood): add atomic counters
- `DBG_WAKE6` (incremented in `wake_by_slot` when `slot == 6`),
- `DBG_PICK6` (incremented in `pick_next` when it returns slot 6 on core 0),
and dump `wake6 / pick6 / TASK_STATE[6]` once every ~128 timer ticks (~6 s) from core 1 via a
single bounded `kprintln`. This answers: is the sender ever woken? ever scheduled? — without
touching the confounded lockfree-serial path.

---

## Cross-references

- `milestones/v2/MILESTONE.md` — the ring-3 AMD userspace milestone (the working baseline).
- `docs/hw-bare-metal-freeze-j5005.md` — earlier Goldmont+ (Wyse 5070) APIC freeze (different
  hardware; the TSC-deadline path is a related APIC workaround).
- Branch `debug/bug-a-bsp-timer` (commit `3f69a3b`) — all the diagnostic probes (irrwatch,
  per-core DIAG, APIC0 readback, LOCKSTUCK detector).
