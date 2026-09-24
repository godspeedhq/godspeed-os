# 48 - `chaos max-carnage` panics the kernel on the kill-path bound, and userspace can reach it

**Opened:** 2026-09-24
**Status:** OPEN - **REPRODUCED 2026-09-24 under host load** (1 panic in 170 carnage rounds loaded; 0 in 400 idle). The cause is understood; what to DO about it is a design decision and is the operator's.
**Found by:** verification of an unrelated change (`feat/stdlib`, which makes **zero** kernel edits).

## What happened

```
gsh> chaos max-carnage all-services 5
...
cap::get: ResourceId(124) gen mismatch cap=75 rec=76 liveness=Dead
KERNEL PANIC: panicked at kernel\src\task\scheduler.rs:2679:25:
kill: core 2 has not released task slot 5 after 1942859025 counter ticks
  (CORE_CURRENT=224, CORE_LEAVING=5). It is running or leaving a task this kill must reclaim and is
  not making progress, so this core cannot safely free the stack and page tables - and will not wait
  forever pretending it can.
```

`osdev test shell` then reported `194 passed, 12 failed` - eleven of those twelve are the harness
losing its place after the machine stopped answering, which is the documented cascade shape. The one
real event is the panic.

## What this is NOT

**It is not a regression from the branch that found it.** `git diff --stat main...HEAD -- kernel/`
is empty, so `feat/stdlib` adds nothing to the kernel and nothing in the change under test can have
altered the kill path.

That is a NARROWER claim than "the kernel is unchanged", and the two were run together once in this
entry's first draft. Whether recent work on MAIN moved the kill path is a separate question with a
separate answer - see Provenance below, where it is checked rather than assumed.

**It is not the guard being wrong to exist.** The message is the guard working exactly as written: a
core that will not release a slot cannot have its stack and page tables freed underneath it, and
waiting forever while pretending otherwise is the silent hang this project refuses. Panicking loudly
is the correct second choice. `backlog/17` records this bound landing as one of the three neutral
kernel changes that closed the riscv64 chaos wedge, hardware-verified on three memory models with
"zero kill-path panics".

## Reproduced, with a measurement

`osdev test chaos-repro[:rounds[:iters]]` loops `chaos max-carnage` inside ONE boot. The original
sighting was one panic in four `osdev test shell` runs - about 1 in 20 carnage rounds - and each of
those runs costs six minutes and spends nearly all of it on things that are not carnage. This does
nothing but carnage, so the rounds per minute go up by about two orders of magnitude and the question
"does host load move the rate" can actually be asked.

It watches for `KERNEL PANIC` as well as the normal end marker, because after a panic the machine
halts (`-no-reboot`) and the serial goes quiet - so a single-marker wait burns its whole window and
then reports only "timed out", which is exactly what made the first sighting read as a harness
cascade rather than the kernel fault it was.

| host | carnage rounds | panics |
|---|---|---|
| idle | 400 | **0** |
| 16 busy processes on 8 cores | 170 | **1** |

Same signature both times, and the second one names a different slot, so it is not slot-specific:

```
KERNEL PANIC: kernel\src\task\scheduler.rs:2679:25
kill: core 2 has not released task slot 1 after 2284108050 counter ticks
  (CORE_CURRENT=224, CORE_LEAVING=1)
```

The load also announced itself before the guest even booted: the first loaded attempt died with
`could not connect to QEMU serial port` because QEMU could not get enough CPU to open its listening
socket inside ten seconds. The harness's windows were widened so the loaded run measures the guest
rather than the harness's patience.

## What it is: a core stopped inside a few-instruction window

`CORE_CURRENT=224` is `IDLE` - `const IDLE: usize = MAX_TASKS`, `MAX_TASKS = 224`, and the constant's
own comment reads "Sentinel meaning 'no task running'". So core 2 had left the dying task and was
still claiming to be leaving it.

`core_release_current(cid, IDLE)` sets both fields together - `CORE_LEAVING = <slot>`, `CORE_CURRENT
= IDLE` - and the next pass through the scheduler loop's top calls `core_finished_leaving()`, which
clears it. **That window is a few instructions wide.** For it to persist 0.75 s, core 2 must have been
STOPPED inside it, which is what a descheduled vCPU thread looks like. The measurement agrees: the
window is hit when the host is oversubscribed and not when it is idle.

### A mechanism that was formed and does NOT hold, recorded so it is not re-derived

It is tempting, and wrong, to blame the idle tick. The arithmetic looks perfect:

- kill-path bound: `liveness_deadline_cycles() / 4` = `(ticks_per_10ms * 300) / 4` = 75 quanta = 0.75 s
- an idle core's own tick: `IDLE_QUANTUM_MULT` = 100 quanta = 1.00 s

75 < 100, so the kill path would panic a quarter-second before a sleeping core's timer was due to
clear the claim - and `2284108050 / 0.75 s` is about 3.0 GHz, which fits. The two constants even
landed two months apart (`06d4016b` 2026-07-19 and `54c95c8d` 2026-09-11), each reasoned against the
300-quanta liveness watchdog and never against each other.

**It is still wrong, because a halted core does not hold the claim.** The idle path halts AFTER the
loop top has cleared it. A sleeping core is not the state the panic printed; a stopped one is.

The 75-vs-100 ordering is a real hazard and worth fixing on its own terms - it just is not this.

## The question this raises, which is bigger than the bug

**Something above the kernel can halt the machine.** `chaos` holds `service_control`, asks for a
service to be killed - entirely legitimate, contracted authority - and the machine dies. No malice and
no exotic input: legal kill requests plus a busy host.

Nothing wrote into ring 0. The kill runs IN the kernel, and it panics on its own invariant: it must
free the dying task's kernel stack and page tables, another core's `CORE_LEAVING` still claims that
slot, and freeing then is a use-after-free of memory a core may still be executing on. At THAT point
the panic is the right of two bad choices - a loud death beats silent ring-0 corruption (26.7,
invariant 12).

The defect is one level up from the panic. **A kill that cannot complete should fail the KILL, not the
MACHINE.** Refuse the reclaim, leave the slot un-freed, report it loudly, and let the supervisor retry
or an operator see a leaked slot. The cost is one task's frames; the thing bought is the invariant
that only the kernel is unkillable. That is 26.7's shape - degrade and record - applied to the one
path that currently cannot.

Two cheaper mitigations, neither of which needs that decision:

- **The `WAKE_RECEIVER` IPI is sent ONCE, before the spin loop, and never re-sent inside it.** A core
  that misses it - or that is not running to take it - is never poked again. Re-sending every few
  thousand iterations costs nothing when the core is merely slow.
- **The bound cannot tell "not scheduled" from "not progressing", and the kernel already knows which.**
  `note_irq` stamps a per-core interrupt count. A core whose count is still advancing is alive and
  slow and should be waited for; a core whose count is frozen is the case this guard is actually for.
  The panic message prints neither.

## Provenance: was this introduced by recent work?

Asked directly, and checked rather than assumed. `feat/stdlib` adds nothing to `kernel/`. But the
work in the gsfs window DID touch the kernel - five commits since 2026-09-17 over six files, about 68
lines: `build.rs` twice (embedded-service list, rebuild trigger), `main.rs` (help/docs, 14 lines),
`task/mod.rs` (constructing `PlacementInvalid`, `backlog/01` - the spawn path), `arch/arm/mod.rs`
(two lines of an `ls` -> `dir` rename) and `arch/riscv64/{mod,sbi}.rs` (reboot, not x86).

**None is in the kill path.** `kernel/src/task/scheduler.rs` last changed 2026-09-13, and the three
commits that shaped this code are 2026-09-10/11 - `f86a5b55`, `25d30ab2` and `54c95c8d`, the riscv64
chaos-wedge work recorded in `backlog/17`. So the bound is as it was when that work was
hardware-verified across three memory models with "zero kill-path panics"; what is new is a host
contended enough to stop a vCPU for three quarters of a second, which no hardware run had.

## What is still open

1. **The decision**: fail the kill, or keep panicking. That is the operator's call, and it is the
   whole of what is left here - the mechanism is no longer in doubt.
2. **On real hardware this may be unreachable.** Every reproduction is under TCG, where a vCPU is a
   host thread that can simply stop. A physical core does not get descheduled. That does not make it
   a test artifact - a false panic under a busy CI host is still a machine that died - but it does
   change how urgent it is, and it should be said rather than left implied.
3. **The panic message should print the stuck core's IRQ count**, per the second mitigation above. It
   is the one number that separates the two explanations, and it would have settled this in one
   sighting instead of two runs and a code read.
