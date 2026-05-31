# Bug 1 — Findings: AP→BSP wake IPI is sent-but-not-serviced (misrouting refuted)

**Date:** 2026-05-30 · **Hardware:** AMD GX-420GI (HP T630) · **Branch:** `debug/bug-a-bsp-timer` @ `f07d7aa`
**Parent bug:** `1_CROSS_CORE_IPC_REPLY_TO_BSP_STALLS.md` (on `main` @ `d74db3d`)
**Purpose:** relay package for external analysis — captures the decisive `lapidmap` + `ipidiag` result.

---

## Context

A blocking `recv` on **core 0 (BSP)** never receives a reply enqueued by an AP; **BSP→AP works,
AP→BSP fails**, deterministically (0/1000 in `bp2-only`; occasionally rescued under heavy churn).
A prior probe (`irrwatch`) proved the BSP's **own LVT timer** fires and latches into IRR exactly
like the APs — so it is not a dead-APIC issue. The leading hypothesis was that the `WAKE_RECEIVER`
IPI to the BSP is **misrouted** (wrong destination APIC id, possibly via the `lapic_to_core_id`
"returns 0 if no match" fallback).

## Probes added (memory-based; the serial path was fixed first)

Serial fix prerequisite: `serial_write_byte` had an **unbounded THRE poll** and
`serial_write_bytes_lockfree` **bypassed `SERIAL_LOCK`** — together they induced false IF=0 serial
wedges that had been contaminating earlier diagnostics. Both now bound the THRE poll (drop byte on
timeout); the lockfree writer best-effort-acquires `SERIAL_LOCK`. So the counters below are trustworthy.

- `IPI_SENT_TO[core]` — incremented in `send_ipi`, indexed by **target** logical core, right before the ICR write.
- `IPI_RECVD[core]` — incremented in the `WAKE_RECEIVER` ISR path (a small Rust wrapper, `ipi_wake_received`, that the stub now calls), indexed by the **receiving** core.
- Boot dump comparing Limine's `bsp_lapic_id`, the BSP's hardware APIC id (`get_lapic_id()`), and the full `CORE_LAPIC_ID` map.

## Results (clean boot: `init: ready`, `supervisor: ready`, ping→pong to 42,458, no panic/PF)

```
lapidmap: bsp_lapic=16 bsp_hw=16 map=[16,17,18,19]
ipidiag:  sent=[1,3,0,0]  recvd=[0,3,0,0]
```

## Interpretation

1. **Destination is correct — misrouting refuted.** `bsp_lapic_id (16) == get_lapic_id() (16)`,
   and `CORE_LAPIC_ID[0] = 16`. `send_ipi(0)` writes `ICR_HIGH = (16 & 0xFF) << 24`,
   `ICR_LOW = vector | (1<<14)` → fixed delivery, **physical** dest mode, edge, assert, dest = APIC 16 = BSP.
2. **The AP services IPIs:** core 1 — 3 sent → 3 received.
3. **The BSP does not:** core 0 — the one `WAKE_RECEIVER` IPI was correctly addressed and sent,
   but `recvd[0] = 0`. The BSP never ran the handler. The IDT is shared and `IDT[0xF0]` is wired
   (the AP services 0xF0 using the same table).

So it is **narrower than "misrouting"**: a physical-mode fixed IPI **AP→BSP** is not taken by the
BSP, even though (a) the destination is correct, (b) the IDT vector is wired, and (c) the BSP's own
LVT timer fires. The BSP is alive throughout (ping runs 42k+).

## Open question (next discriminating bit)

- Does the IPI **reach the BSP's IRR but never get taken** (BSP running `IF=0` in the window) →
  read BSP `IRR[0xF0]` (reg `0x270`, bit 16); set = pending-but-blocked.
- Or does it **never reach the BSP's IRR** (APIC accept/delivery asymmetry for AP→BSP fixed IPIs)?
- And: does the BSP service its **own periodic timer** at runtime (which should backstop a lost wake
  by letting `pick_next` reschedule the now-Ready sender)? If yes, the lost IPI alone shouldn't fully
  stall it → bug shifts toward `pick_next` not selecting the Ready BSP-resident receiver. (Planned
  probe: `TICK_ON[cid]` at the top of `timer_tick_from_irq`.)

## Update 2 (2026-05-30) — timer is HEALTHY; it's an IF=0 kernel wedge of core 0

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

### Static audit of IF=0 paths (no obvious culprit; LOCKSTUCK was incomplete)

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

### Next: NMI-RIP  — BUILT (this flash)

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

## Update 3 (2026-05-30) — the wedge model is WRONG; core 0 is alive

The NMI-RIP probe returned a **null result** (`nmi0: rip=0x0 count=0`), and the log around the
stall shows why: **core 0 is fully alive.** The regular one-way ping→pong runs to
`pong: received "30089"` and climbing, `ipidiag` dumps 19×, `apicst0` counter keeps moving.
Core 0 schedules, preempts, and runs ping the entire time. Update 2's "IF=0 wedge of core 0"
is **refuted** — there is nothing wedged for the NMI to catch.

What actually stalls is one specific task. `bp2-only` runs two independent flows on core 0:
- **ping → pong** (one-way, no reply) — never needs an AP→BSP wake → runs forever. ✓
- **perf-bp2 measurer (BSP) ⇄ perf-bp2-echo (AP)** — round-trip. `perf: BP2 echo sent-0` shows
  echo received msg 0 and **sent its AP→BSP reply**, then nothing. The measurer, blocked on
  `recv` on the BSP, **never receives reply 0**, so BP2 stalls at iteration 0.

Smoking gun: `ipidiag: sent=[0,3,0,0]` — `IPI_SENT_TO[0] = 0`. When echo enqueued its reply for
the BSP-blocked measurer, the `WAKE_RECEIVER` IPI to core 0 was **never even sent** (run-to-run
it is sometimes sent-once-not-serviced, sometimes not sent at all → a **race**, not a dead APIC
and not an IF=0 wedge). So the bug is in the **send→wake handshake / routing**, not core 0's
health, and it is isolated to **AP→BSP** wakeups (BSP→AP via ping→pong works: `sent[1]=recvd[1]=3`).

The lock-serialised enqueue/dequeue handshake and `block_and_reschedule`'s CAS both *look*
correct on audit, so rather than theorise: **`rtdiag` probe** — routing outcomes keyed by the
endpoint's owning core (index 0 = measurer's BSP reply endpoint, isolated from core-1 ping→pong),
dumped from pong's `handle_recv` on core 1 (a reliable busy-core dump site; the idle loop is not,
which is why only one `nmi0` line printed). Watch:
`rtdiag c0: woke=W queued=Q dead=D blk=B msg=M sent=S recvd=R || c1: ...`
- `woke` climbs but `sent`(=IPI_SENT_TO[0]) stays 0 → `wake_by_slot` misclassifies the core.
- `queued` climbs but `msg` stays 0 → replies pile in the measurer's queue, never dequeued.
- `dead` climbs → echo's reply hits a dead/gen-mismatch endpoint.

## Update 4 (2026-05-30) — echo's reply is CORRECT; the delay is STARVATION

A long instrumentation chain (per-slot send/recv endpoint map, per-core entry
counts, raw-LAPIC histogram, and finally a synchronous immediate log) converged:

```
ECHOTX: lapic=17 core=1 slot=7 capslot=3 ep=104 epcore=0
```

echo's reply runs on **core 1, slot 7** (correct — no core misread, no migration),
uses the **correct cap (slot 3)**, and targets **endpoint 104 on core 0** — exactly the
sender's recv endpoint. So the "endpoint mismatch", "core-id misread", and "cap-wiring"
theories are all **refuted**. The reply is correctly addressed AP→BSP.

The real symptom: echo replies **once, very late** — `echo recv-0` at log line 118,
`ECHOTX`/`echo sent-0` at line ~307, with ~189 lines of ping→pong + dumps in between.
echo shares **core 1 with pong**, and ping floods pong (30k+ msgs), so **echo is starved
of scheduler quanta** and its `try_send` retry loop barely advances. Every prior
`c0 sent=0`/`woke=0` was recorded *before* echo's single late send — they did not prove
a wake failure, only that echo hadn't replied yet. The whole "AP→BSP wake never sent"
framing was an artifact of sampling before the send.

Open question (next probe): when echo's late reply enqueues to ep 104 and the sender is
blocked there, does the wake fire and the sender receive? Immediate logs added:
`WAKE0:` (any wake of a core-0 task — slot/task_core/my_core/cross) and
`SENDER_GOT_REPLY:` (sender slot 6 dequeues). If WAKE0 shows cross=true and the sender
still never logs recv-0, the AP→BSP wake is real; if SENDER_GOT_REPLY appears, BP2 is
merely glacially slow under pong starvation, not stuck.

## Update 5 (2026-05-30) — CONFIRMED: AP->BSP wake IPI sent but never serviced by BSP

With echo's reply now proven correct, immediate synchronous logs captured the whole chain
the instant it happens:

```
ECHOTX: lapic=17 core=1 slot=7 capslot=3 ep=104 epcore=0   ← echo's reply, correctly addressed
WAKE0:  slot=6 task_core=0 my_core=1 cross=true result=0   ← wake_by_slot fires send_ipi(0, 0xF0)
perf: BP2 echo sent-0
(silence — IPIRECV0 never appears; IPIRECV0 count = 0)
```

`IPIRECV0` is logged at the very top of `ipi_wake_received` (core 0's WAKE_RECEIVER handler),
before the reschedule. It NEVER fires → **core 0 never vectors into IDT[0xF0]**. So the AP(core1)
→BSP(core0) `WAKE_RECEIVER` IPI is correctly **sent** (`WAKE0 cross=true`, `send_ipi(0,0xF0)`) but
**never serviced** by the BSP. Everything upstream is correct (echo reply target ep=104 core 0,
cap slot 3, wake decision cross=true). The failure is purely **AP→BSP IPI acceptance at the BSP**.
Output also stops entirely after this point (pong on core 1 ceases) — consistent with a hard wedge
once the wake IPI is fired at core 0.

Next probe (built): core 1's idle loop NMIs core 0 (NMI ignores IF), and core 0's NMI handler
samples its OWN `IRR[0xF0]` + RIP. `nmi0: rip=.. count=.. irrf0=..`:
- `irrf0=1` ⇒ the wake IPI reached core 0's IRR but was never taken → core 0 stuck IF=0 (wedge).
- `irrf0=0` with `count>0` ⇒ NMI delivered but no pending 0xF0 → the wake IPI never arrived
  (APIC accept asymmetry AP→BSP for fixed IPIs).
- `count=0` ⇒ core 0 takes neither NMI nor IPI (fully hung).

## Update 6 (2026-05-30) — ROOT CAUSE FOUND + FIXED: unbounded COM2 drain in core 0's timer ISR

NMI-into-core-0 (which ignores IF) caught the wedge directly:

```
(healthy, pre-reply) nmi0: rip=0xffffffff80108c38 irrf0=0 ipi_sent0=0   ← serial_write_byte
(wedged, post-reply) nmi0: rip=0xffffffff801078ae irrf0=1 ipi_sent0=1 ipi_recvd0=0
                     nmi0: rip=0xffffffff80107c44 irrf0=1 ...            ← alternating, stuck
```

- `irrf0=1` ⇒ the AP→BSP wake IPI **does** reach core 0's IRR — **APIC delivery is fine**.
- `ipi_recvd0=0` + NMI `count` climbing ⇒ core 0 takes NMIs but never the 0xF0 ⇒ **IF=0**.
- Both wedge RIPs map (via `rust-nm`) into **`kernel::control::process_pending`** (sym 0x80107832).

`process_pending` (control.rs) drained COM2 with an **unbounded** `while let Some(b) =
com2_try_read_byte()` loop, and it is called from `timer_tick_from_irq` (scheduler.rs:790) on
every core-0 tick **with IF=0**. `com2_try_read_byte` returns `Some` whenever COM2's LSR
Data-Ready bit (port COM2+5, bit 0) is set — and on the HP T630 **there is no usable COM2, so
that port floats to 0xFF**, leaving DR permanently set. The loop never terminates → core 0 spins
IF=0 inside the timer ISR → it can never take the latched 0xF0 WAKE_RECEIVER IPI → the perf-bp2
sender blocked on `recv` on the BSP is never woken → BP2 stalls (and the system hard-wedges).

**The entire "AP→BSP wake never serviced" symptom was a downstream effect of this IF=0 wedge,
not an APIC/IPI bug.** BSP→AP worked only because the AP's timer ISR never wedged (a real COM2
isn't polled there).

### Fix

control.rs `process_pending`: bound the drain loop to 256 iterations per call (≫ BUF_SIZE=128).
A stuck/floating COM2 now drains at most 256 junk bytes and returns, so the timer ISR always
completes and core 0 stays interruptible. Status: built, pending hardware verification (expect
`sender recv-0 OK`, IPIRECV0 firing, BP2 completing).

## Code facts for the analyst

- `send_ipi` (smp/ipi.rs): writes `ICR_HIGH` (dest) first, polls Delivery-Status (ICR_LOW bit 12,
  capped 10k iters), then writes `ICR_LOW = vector | (1<<14)`. Each core uses its own APIC MMIO base.
- xAPIC mode (IDs fit in 8 bits; all APIC access is MMIO). **DFR/LDR (logical destination) not
  configured — delivery is physical mode throughout.**
- `ipi_wake_stub` (IDT[0xF0]) does conditional `swapgs`, saves caller-saved regs, now `call
  ipi_wake_received` (counts receipt, then `timer_tick_from_irq`), restores, conditional `swapgs`, `iretq`.
- `lapic_to_core_id` returns 0 on no-match (fallback) — would mask a wrong `CORE_LAPIC_ID[0]`
  everywhere except the IPI destination; here `CORE_LAPIC_ID[0]` is confirmed correct (16), so the
  fallback is not in play.
