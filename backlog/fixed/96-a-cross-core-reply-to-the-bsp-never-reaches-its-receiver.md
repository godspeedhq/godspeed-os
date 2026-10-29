# 96. Cross-core IPC reply to the BSP (core 0) never reaches a blocked receiver

**Status: RESOLVED 2026-05-31 (`a306fd3`).** Root cause: an unbounded COM2 drain in core 0's timer
ISR with IF=0. The T630 has no usable COM2, so its LSR floats and the drain never terminated - core 0
live-locked in the ISR and the AP->BSP reply never ran. Fixed by bounding the drain to 256
iterations per call. `milestones/hardware/ring3-bringup.md` ("BP2 unblocked") is the landing record,
and `CLAUDE.md` §23.3 cites this file for the investigation; BP2 was measured on the T630 immediately
afterwards. Kept for the investigation, which is the reason the file exists.

*(This header read "Open - confirmed real" for three months after the fix landed, while two other
documents recorded it as closed.)*
**Severity:** High - blocks the cross-core IPC round-trip (BP2 benchmark / any request→reply where the requester is on the BSP)
**Hardware:** HP T630 thin client - AMD GX-420GI (Jaguar/Puma+, Family 22h), 4 cores ~2 GHz, 8 GB RAM. LAPIC IDs 16/17/18/19 (BSP = LAPIC 16 = logical core 0).
**First seen:** 2026-05-29 · last reproduced clean: 2026-05-30
**Affected branch/commit:** reproduces on `main` @ `059b104` (the ring-3 AMD userspace milestone). Debug instrumentation lives on branch `debug/bug-a-bsp-timer`.

---

## One-line summary

A task that performs a blocking `recv` **on core 0 (the BSP)** is never delivered a
message that another core (`AP`) enqueues for it - the receiver stays starved/stalled.
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
  occasionally completes - **4/1000** in one run - but **0/1000** in isolation (`bp2-only`).
  Racy, not a hard failure.

## What runs on which core (bp2-only)

```
core 0: init(0) supervisor(1) registry(2) logger(3) ping(5) perf-bp2/sender(6)
core 1: pong(4) perf-bp2-echo(7)
```
ping (core 0) calls `yield_cpu()` every loop iteration (~40k+ yields observed), so core 0's
`pick_next` runs tens of thousands of times - yet the Ready sender (slot 6) is not serviced
to completion.

---

## Key proven facts (hypotheses RULED OUT)

1. **NOT an APIC timer hardware/delivery failure on the BSP.**
   A deterministic boot-time probe (`irrwatch`, each core spins IF=0 until 2 timer reloads,
   watching `APIC_TIMER_CURRENT` and `IRR[0x20]`) reported **identical** results on all four cores
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
   reproduces the stall exactly. (However the diagnostics DID confound other runs - see below.)

5. **NOT a `SpinLock<T>` deadlock.** A stuck-lock detector (prints `LOCKSTUCK <addr>` after
   80M spins inside `SpinLock::lock`) never fired during the stall.

---

## Current best understanding (unproven)

At runtime, **core 0 services no interrupts** while stalled (`c0=0`: `timer_tick_from_irq`
ran 0 times on logical core 0 across many runs, while core 1 logged dozens). Since the timer
provably *fires* (fact 1), the only explanation is that **core 0 runs with `IF=0` (interrupts
disabled) for long/indefinite stretches**, so the pending timer (and any `WAKE_RECEIVER` IPI)
is never taken - hence no preemption and no cross-core wake servicing on the BSP.

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

QEMU note: never reproduces under QEMU TCG (which serializes cores) - real-SMP only. This
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

- `kernel/src/syscall/dispatch.rs` - `handle_recv` (block loop), `handle_send`/`handle_try_send`.
- `kernel/src/ipc/routing.rs` - `enqueue_locked` / `dequeue_locked` (blocked_receiver recording,
  the `TABLE` SpinLock), `endpoint_queue_depth`.
- `kernel/src/task/scheduler.rs` - `block_and_reschedule`, `wake_by_slot` (cross-core CAS-retry
  + `CORE_WAKE_HINT` + `send_ipi`), `pick_next` (hint fast-path + RR scan), `yield_current`,
  `timer_tick_from_irq`.
- `kernel/src/smp/ipi.rs` - `send_ipi` (ICR write + DELIVS poll), `WAKE_RECEIVER` (0xF0) path.
- `kernel/src/arch/x86_64/boot.rs` - `ipi_wake_stub` (IDT[0xF0] → `timer_tick_from_irq`),
  `init_local_apic` (TPR zero, LVT timer, periodic vs TSC-deadline), `apic_send_eoi`.
- `kernel/src/smp/core.rs` - `lapic_to_core_id`, `current_core_id` source.
- `kernel/src/arch/x86_64/mod.rs` - `serial_write_byte` (unbounded THRE), `serial_write_bytes_lockfree`
  (SERIAL_LOCK bypass) - the confound / latent bug.

---

## Open questions for research

1. On AMD Family 22h (Jaguar), are there known issues delivering a fixed-vector **IPI to the
   BSP**, or with the BSP servicing its periodic LVT timer, that differ from APs? (We proved
   the timer *fires into IRR*; the question is why the BSP appears to run `IF=0` so long that it
   never takes it.)
2. Is there a kernel path on the `recv`/reply side that disables interrupts (`cli`, interrupt-gate
   syscall entry, a spinlock acquired with IF=0) and can spin indefinitely on real SMP - i.e. a
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
single bounded `kprintln`. This answers: is the sender ever woken? ever scheduled? - without
touching the confounded lockfree-serial path.

---

## Cross-references

- `milestones/hardware/ring3-bringup.md` - the ring-3 AMD userspace milestone (the working baseline).
- `docs/hw-bare-metal-freeze-j5005.md` - earlier Goldmont+ (Wyse 5070) APIC freeze (different
  hardware; the TSC-deadline path is a related APIC workaround).
- Branch `debug/bug-a-bsp-timer` (commit `3f69a3b`) - all the diagnostic probes (irrwatch,
  per-core DIAG, APIC0 readback, LOCKSTUCK detector).

---

*Folded into this entry from `bugs/1_FINDINGS_AP_TO_BSP_IPI.md` on 2026-10-29, when `bugs/` joined the backlog.*

## Appendix: the findings relay - AP→BSP wake IPI is sent-but-not-serviced (misrouting refuted)

**Date:** 2026-05-30 · **Hardware:** AMD GX-420GI (HP T630) · **Branch:** `debug/bug-a-bsp-timer` @ `f07d7aa`
**Parent bug:** this entry, above (on `main` @ `d74db3d`)
**Purpose:** relay package for external analysis - captures the decisive `lapidmap` + `ipidiag` result.

---

### Context

A blocking `recv` on **core 0 (BSP)** never receives a reply enqueued by an AP; **BSP→AP works,
AP→BSP fails**, deterministically (0/1000 in `bp2-only`; occasionally rescued under heavy churn).
A prior probe (`irrwatch`) proved the BSP's **own LVT timer** fires and latches into IRR exactly
like the APs - so it is not a dead-APIC issue. The leading hypothesis was that the `WAKE_RECEIVER`
IPI to the BSP is **misrouted** (wrong destination APIC id, possibly via the `lapic_to_core_id`
"returns 0 if no match" fallback).

### Probes added (memory-based; the serial path was fixed first)

Serial fix prerequisite: `serial_write_byte` had an **unbounded THRE poll** and
`serial_write_bytes_lockfree` **bypassed `SERIAL_LOCK`** - together they induced false IF=0 serial
wedges that had been contaminating earlier diagnostics. Both now bound the THRE poll (drop byte on
timeout); the lockfree writer best-effort-acquires `SERIAL_LOCK`. So the counters below are trustworthy.

- `IPI_SENT_TO[core]` - incremented in `send_ipi`, indexed by **target** logical core, right before the ICR write.
- `IPI_RECVD[core]` - incremented in the `WAKE_RECEIVER` ISR path (a small Rust wrapper, `ipi_wake_received`, that the stub now calls), indexed by the **receiving** core.
- Boot dump comparing Limine's `bsp_lapic_id`, the BSP's hardware APIC id (`get_lapic_id()`), and the full `CORE_LAPIC_ID` map.

### Results (clean boot: `init: ready`, `supervisor: ready`, ping→pong to 42,458, no panic/PF)

```
lapidmap: bsp_lapic=16 bsp_hw=16 map=[16,17,18,19]
ipidiag:  sent=[1,3,0,0]  recvd=[0,3,0,0]
```

### Interpretation

1. **Destination is correct - misrouting refuted.** `bsp_lapic_id (16) == get_lapic_id() (16)`,
   and `CORE_LAPIC_ID[0] = 16`. `send_ipi(0)` writes `APIC_ICR_HIGH = (16 & 0xFF) << 24`,
   `ICR_LOW = vector | (1<<14)` → fixed delivery, **physical** dest mode, edge, assert, dest = APIC 16 = BSP.
2. **The AP services IPIs:** core 1 - 3 sent → 3 received.
3. **The BSP does not:** core 0 - the one `WAKE_RECEIVER` IPI was correctly addressed and sent,
   but `recvd[0] = 0`. The BSP never ran the handler. The IDT is shared and `IDT[0xF0]` is wired
   (the AP services 0xF0 using the same table).

So it is **narrower than "misrouting"**: a physical-mode fixed IPI **AP→BSP** is not taken by the
BSP, even though (a) the destination is correct, (b) the IDT vector is wired, and (c) the BSP's own
LVT timer fires. The BSP is alive throughout (ping runs 42k+).

### Open question (next discriminating bit)

- Does the IPI **reach the BSP's IRR but never get taken** (BSP running `IF=0` in the window) →
  read BSP `IRR[0xF0]` (reg `0x270`, bit 16); set = pending-but-blocked.
- Or does it **never reach the BSP's IRR** (APIC accept/delivery asymmetry for AP→BSP fixed IPIs)?
- And: does the BSP service its **own periodic timer** at runtime (which should backstop a lost wake
  by letting `pick_next` reschedule the now-Ready sender)? If yes, the lost IPI alone shouldn't fully
  stall it → bug shifts toward `pick_next` not selecting the Ready BSP-resident receiver. (Planned
  probe: `TICK_ON[cid]` at the top of `timer_tick_from_irq`.)

### Update 2 (2026-05-30) - timer is HEALTHY; it's an IF=0 kernel wedge of core 0

Two more probes (memory-based; serial fix in place):

- `ipidiag` (IPI send/recv + `TICK_ON` per core + `bsp_irr_f0`): over a run it settled at
  `sent=[0,3,0,0] recvd=[0,3,0,0] tick=[0,3,0,0] bsp_irr_f0=0`. Core 1 services IPIs
  (3 sent→3 recvd); core 0 services 0 timer ticks and 0 IPIs. (Run-to-run, the wake IPI to
  the BSP is sometimes sent (sent[0]=1, prior run) and sometimes not even sent (sent[0]=0).)
- `apicst0` (core 0's OWN runtime APIC timer state, 17 samples):
  `tpr=0x0  lvt=0x20020  cur1=4799129..197125 (decreasing) cur2=cur1-~668`.
  => **TPR=0 (not blocking), LVT periodic+unmasked (0x20020), counter MOVING.** The timer
  hardware/config is healthy. TPR/mask/stopped-timer hypotheses all REFUTED.

Key correction: the 17 dumps span only ~34 ms (< the 50 ms period) and `cur` decreases
**monotonically without reloading**, because the dumps are gated on core-0 `yield_current`,
which STOPS when core 0 wedges (~34 ms). So the earlier "no core services its timer" was
partly a short-capture artifact (the timer simply hadn't fired yet). Corrected picture:

1. ping runs ~34 ms (yielding); the BSP-resident sender is starved/Ready.
2. The sender (or a task on core 0) **wedges with IF=0 in the recv path at ~34 ms**, before
   the first 50 ms timer fire, and monopolizes the core.
3. Thereafter the healthy timer fires into core 0's IRR every 50 ms but is **never taken
   (IF=0)**. No preemption → permanent stall. **Bug = an IF=0 kernel wedge of core 0.**

#### Static audit of IF=0 paths (no obvious culprit; LOCKSTUCK was incomplete)

- `LOCKSTUCK` only instruments `SpinLock<T>::lock`. **Two hand-rolled / unbounded IF=0 spins
  are invisible to it:** `task_slot_lock` (raw `AtomicBool` CAS-spin, unbounded) and the
  TLB-shootdown `TLB_ACK` spin (`while TLB_ACK < expected { spin_loop }`, unbounded). So the
  earlier "no LOCKSTUCK" did NOT exclude these.
- In `bp2-only` neither should trigger at runtime: `task_slot_lock` is spawn/kill-only (spawns
  happen at boot, no restart storm); `TLB_ACK` only on a task-death unmap (no kills). The
  recv-path locks actually hit (`GLOBAL_RESOURCES`, routing `TABLE`) are `SpinLock<T>`
  (instrumented, silent). `block_and_reschedule`/`handle_recv` IF handling reads correct.
- Conclusion: no obvious unbounded IF=0 spin in the bp2-only recv path → likely a subtle
  real-SMP race, or an unexpected trigger of one of the blind-spot spins.

#### Next: NMI-RIP  - BUILT (this flash)

Core 1 sends core 0 an NMI (delivered even with IF=0); core 0's NMI handler records its
interrupted RIP; map via `rust-nm` to the exact wedged instruction. Definitive regardless of
IF state or lock type. Bundled with stuck-detectors on the two LOCKSTUCK blind spots
(`task_slot_lock`, `TLB_ACK`) so one flash either pins the RIP or flags the exact spin.

Serial line to watch (emitted from core 1's idle loop):
`nmi0: rip=<hex> count=<n> tlbstuck=<0|1> tslotstuck=<0|1>`
- `count` climbing + `rip` pinned to one value → that RIP is the wedge (map it).
- `tlbstuck=1` / `tslotstuck=1` → wedge is that specific hand-rolled IF=0 spin.
- `rip` varying / `count=0` → core 0 not wedged when NMI'd → reframes the model.

Symbols (release ELF): `nmi_stub`=0xffffffff801012b8, `nmi_record`=0xffffffff8010c3ab.

### Update 3 (2026-05-30) - the wedge model is WRONG; core 0 is alive

The NMI-RIP probe returned a **null result** (`nmi0: rip=0x0 count=0`), and the log around the
stall shows why: **core 0 is fully alive.** The regular one-way ping→pong runs to
`pong: received "30089"` and climbing, `ipidiag` dumps 19×, `apicst0` counter keeps moving.
Core 0 schedules, preempts, and runs ping the entire time. Update 2's "IF=0 wedge of core 0"
is **refuted** - there is nothing wedged for the NMI to catch.

What actually stalls is one specific task. `bp2-only` runs two independent flows on core 0:
- **ping → pong** (one-way, no reply) - never needs an AP→BSP wake → runs forever. ✓
- **perf-bp2 measurer (BSP) ⇄ perf-bp2-echo (AP)** - round-trip. `perf: BP2 echo sent-0` shows
  echo received msg 0 and **sent its AP→BSP reply**, then nothing. The measurer, blocked on
  `recv` on the BSP, **never receives reply 0**, so BP2 stalls at iteration 0.

Smoking gun: `ipidiag: sent=[0,3,0,0]` - `IPI_SENT_TO[0] = 0`. When echo enqueued its reply for
the BSP-blocked measurer, the `WAKE_RECEIVER` IPI to core 0 was **never even sent** (run-to-run
it is sometimes sent-once-not-serviced, sometimes not sent at all → a **race**, not a dead APIC
and not an IF=0 wedge). So the bug is in the **send→wake handshake / routing**, not core 0's
health, and it is isolated to **AP→BSP** wakeups (BSP→AP via ping→pong works: `sent[1]=recvd[1]=3`).

The lock-serialised enqueue/dequeue handshake and `block_and_reschedule`'s CAS both *look*
correct on audit, so rather than theorise: **`rtdiag` probe** - routing outcomes keyed by the
endpoint's owning core (index 0 = measurer's BSP reply endpoint, isolated from core-1 ping→pong),
dumped from pong's `handle_recv` on core 1 (a reliable busy-core dump site; the idle loop is not,
which is why only one `nmi0` line printed). Watch:
`rtdiag c0: woke=W queued=Q dead=D blk=B msg=M sent=S recvd=R || c1: ...`
- `woke` climbs but `sent`(=IPI_SENT_TO[0]) stays 0 → `wake_by_slot` misclassifies the core.
- `queued` climbs but `msg` stays 0 → replies pile in the measurer's queue, never dequeued.
- `dead` climbs → echo's reply hits a dead/gen-mismatch endpoint.

### Update 4 (2026-05-30) - echo's reply is CORRECT; the delay is STARVATION

A long instrumentation chain (per-slot send/recv endpoint map, per-core entry
counts, raw-LAPIC histogram, and finally a synchronous immediate log) converged:

```
ECHOTX: lapic=17 core=1 slot=7 capslot=3 ep=104 epcore=0
```

echo's reply runs on **core 1, slot 7** (correct - no core misread, no migration),
uses the **correct cap (slot 3)**, and targets **endpoint 104 on core 0** - exactly the
sender's recv endpoint. So the "endpoint mismatch", "core-id misread", and "cap-wiring"
theories are all **refuted**. The reply is correctly addressed AP→BSP.

The real symptom: echo replies **once, very late** - `echo recv-0` at log line 118,
`ECHOTX`/`echo sent-0` at line ~307, with ~189 lines of ping→pong + dumps in between.
echo shares **core 1 with pong**, and ping floods pong (30k+ msgs), so **echo is starved
of scheduler quanta** and its `try_send` retry loop barely advances. Every prior
`c0 sent=0`/`woke=0` was recorded *before* echo's single late send - they did not prove
a wake failure, only that echo hadn't replied yet. The whole "AP→BSP wake never sent"
framing was an artifact of sampling before the send.

Open question (next probe): when echo's late reply enqueues to ep 104 and the sender is
blocked there, does the wake fire and the sender receive? Immediate logs added:
`WAKE0:` (any wake of a core-0 task - slot/task_core/my_core/cross) and
`SENDER_GOT_REPLY:` (sender slot 6 dequeues). If WAKE0 shows cross=true and the sender
still never logs recv-0, the AP→BSP wake is real; if SENDER_GOT_REPLY appears, BP2 is
merely glacially slow under pong starvation, not stuck.

### Update 5 (2026-05-30) - CONFIRMED: AP->BSP wake IPI sent but never serviced by BSP

With echo's reply now proven correct, immediate synchronous logs captured the whole chain
the instant it happens:

```
ECHOTX: lapic=17 core=1 slot=7 capslot=3 ep=104 epcore=0   ← echo's reply, correctly addressed
WAKE0:  slot=6 task_core=0 my_core=1 cross=true result=0   ← wake_by_slot fires send_ipi(0, 0xF0)
perf: BP2 echo sent-0
(silence - IPIRECV0 never appears; IPIRECV0 count = 0)
```

`IPIRECV0` is logged at the very top of `ipi_wake_received` (core 0's WAKE_RECEIVER handler),
before the reschedule. It NEVER fires → **core 0 never vectors into IDT[0xF0]**. So the AP(core1)
→BSP(core0) `WAKE_RECEIVER` IPI is correctly **sent** (`WAKE0 cross=true`, `send_ipi(0,0xF0)`) but
**never serviced** by the BSP. Everything upstream is correct (echo reply target ep=104 core 0,
cap slot 3, wake decision cross=true). The failure is purely **AP→BSP IPI acceptance at the BSP**.
Output also stops entirely after this point (pong on core 1 ceases) - consistent with a hard wedge
once the wake IPI is fired at core 0.

Next probe (built): core 1's idle loop NMIs core 0 (NMI ignores IF), and core 0's NMI handler
samples its OWN `IRR[0xF0]` + RIP. `nmi0: rip=.. count=.. irrf0=..`:
- `irrf0=1` ⇒ the wake IPI reached core 0's IRR but was never taken → core 0 stuck IF=0 (wedge).
- `irrf0=0` with `count>0` ⇒ NMI delivered but no pending 0xF0 → the wake IPI never arrived
  (APIC accept asymmetry AP→BSP for fixed IPIs).
- `count=0` ⇒ core 0 takes neither NMI nor IPI (fully hung).

### Update 6 (2026-05-30) - ROOT CAUSE FOUND + FIXED: unbounded COM2 drain in core 0's timer ISR

NMI-into-core-0 (which ignores IF) caught the wedge directly:

```
(healthy, pre-reply) nmi0: rip=0xffffffff80108c38 irrf0=0 ipi_sent0=0   ← serial_write_byte
(wedged, post-reply) nmi0: rip=0xffffffff801078ae irrf0=1 ipi_sent0=1 ipi_recvd0=0
                     nmi0: rip=0xffffffff80107c44 irrf0=1 ...            ← alternating, stuck
```

- `irrf0=1` ⇒ the AP→BSP wake IPI **does** reach core 0's IRR - **APIC delivery is fine**.
- `ipi_recvd0=0` + NMI `count` climbing ⇒ core 0 takes NMIs but never the 0xF0 ⇒ **IF=0**.
- Both wedge RIPs map (via `rust-nm`) into **`kernel::control::process_pending`** (sym 0x80107832).

`process_pending` (control.rs) drained COM2 with an **unbounded** `while let Some(b) =
com2_try_read_byte()` loop, and it is called from `timer_tick_from_irq` (scheduler.rs:790) on
every core-0 tick **with IF=0**. `com2_try_read_byte` returns `Some` whenever COM2's LSR
Data-Ready bit (port COM2+5, bit 0) is set - and on the HP T630 **there is no usable COM2, so
that port floats to 0xFF**, leaving DR permanently set. The loop never terminates → core 0 spins
IF=0 inside the timer ISR → it can never take the latched 0xF0 WAKE_RECEIVER IPI → the perf-bp2
sender blocked on `recv` on the BSP is never woken → BP2 stalls (and the system hard-wedges).

**The entire "AP→BSP wake never serviced" symptom was a downstream effect of this IF=0 wedge,
not an APIC/IPI bug.** BSP→AP worked only because the AP's timer ISR never wedged (a real COM2
isn't polled there).

#### Fix

control.rs `process_pending`: bound the drain loop to 256 iterations per call (≫ BUF_SIZE=128).
A stuck/floating COM2 now drains at most 256 junk bytes and returns, so the timer ISR always
completes and core 0 stays interruptible. Status: built, pending hardware verification (expect
`sender recv-0 OK`, IPIRECV0 firing, BP2 completing).

### Code facts for the analyst

- `send_ipi` (smp/ipi.rs): writes `APIC_ICR_HIGH` (dest) first, polls Delivery-Status (ICR_LOW bit 12,
  capped 10k iters), then writes `ICR_LOW = vector | (1<<14)`. Each core uses its own APIC MMIO base.
- xAPIC mode (IDs fit in 8 bits; all APIC access is MMIO). **DFR/LDR (logical destination) not
  configured - delivery is physical mode throughout.**
- `ipi_wake_stub` (IDT[0xF0]) does conditional `swapgs`, saves caller-saved regs, now `call
  ipi_wake_received` (counts receipt, then `timer_tick_from_irq`), restores, conditional `swapgs`, `iretq`.
- `lapic_to_core_id` returns 0 on no-match (fallback) - would mask a wrong `CORE_LAPIC_ID[0]`
  everywhere except the IPI destination; here `CORE_LAPIC_ID[0]` is confirmed correct (16), so the
  fallback is not in play.
