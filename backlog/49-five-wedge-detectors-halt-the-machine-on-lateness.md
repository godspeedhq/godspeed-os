# 49 - five wedge detectors halt the machine on LATENESS, and cannot tell it from a fault

**Opened:** 2026-09-24
**Status:** OPEN - one instance PROVEN (`backlog/48`), the class audited, no fix attempted.
**Raised by:** the operator, ruling on `backlog/48`: *"Nothing above the kernel should have influence
to panic or wedge the kernel. Nothing. If the kernel panics it'll be because of the logic inside the
kernel itself."*

## The rule is already written, twice

This is not a new policy. `CLAUDE.md` 22 states it in the strongest language the document uses:

> Fuzz tests find the inputs that crash the kernel that no one would write by hand. The bar is
> **binary and absolute**: the kernel must never panic on user-controllable input.

> Bar: every attack returns a defined error. Any attack that succeeds is a security hole; any attack
> that **panics the kernel is a kernel bug**. Both are **mandatory fixes**.

`backlog/48` was recorded as though the remedy were a design choice for the operator. It is not. The
constitution already decided it; that entry is corrected.

## The distinction the rule actually draws

The ruling does NOT say a kernel may never panic. It says the panic must come from the kernel's own
logic. That splits the existing panics cleanly:

- **A kernel invariant was violated** - an allocator returned a kernel-range frame, a boot table is
  too small, the supervisor's image will not spawn. The kernel found its own fault. **Allowed**, and
  26.7 says loud is right.
- **Something did not happen fast enough** - another core has not acknowledged, released, or
  progressed within N. That is not a fault the kernel has established. It is *lateness*, and the
  causes of lateness include things the kernel does not control at all.

Every panic below in the second group shares one defect: **it treats "late" as "broken", and halts the
machine on the inference.**

## The audit: 13 `panic!` sites outside `arch/`

**Boot-only, and correct (11.3 makes bootstrap failure fatal):**

| site | what |
|---|---|
| `task/mod.rs:2448` | supervisor spawn failed |
| `ipc/routing.rs:182` | endpoint table full at boot |
| `memory/allocator.rs:144` | HHDM offset not set before init |
| `memory/allocator.rs:183` | largest usable region too small for the frame bitmap |

**A kernel invariant, correct:**

| site | what |
|---|---|
| `memory/allocator.rs:42` | `alloc_frame` returned a kernel-range frame |

**LATENESS - the class this entry is about:**

| site | bound | what it waits on |
|---|---|---|
| `task/scheduler.rs:2679` | `liveness_deadline_cycles()/4` = 75 quanta ~ 0.75 s | another core releasing a task slot - **PROVEN reachable, `backlog/48`** |
| `task/scheduler.rs:1616` | 300 quanta ~ 3 s of ticks | another core making any progress |
| `smp/ipi.rs:232` | `SHOOTDOWN_WATCHDOG_SPINS` = 500,000,000 **spins** | other cores acknowledging a TLB shootdown |
| `smp/spinlock.rs:35` | `LOCK_WATCHDOG_SPINS` **spins** | a lock holder releasing |
| `memory/allocator.rs:470` | `ALLOC_WATCHDOG_SPINS` **spins** | the frame-allocator lock holder releasing - its own message names "preempted holder" as a cause |

Three of the five are bounded by a SPIN COUNT, which is the shape this project has already learned
is not a bound on anything: a count means a different wall time on every machine, and a spinning core
burns iterations fastest exactly when the core it waits on is getting no CPU at all.

**Userspace can drive the resource:**

| site | what | reachable? |
|---|---|---|
| `task/scheduler.rs:879` | run queue full | a spawn storm fills run queues, and `chaos spawn-storm` exists **to do that** |
| `ipc/mod.rs:57` | endpoint id space exhausted (hits the delegated/file-cap band) | every spawn takes an id |
| `capability/generation.rs:59` | generation counter wrapped | ~4.2 billion creations per boot; bounded by an argument the code states |

## Why "just do not panic" is not the answer

Each of these replaced something worse. The kill-path bound replaced an unbounded spin that took the
WAITING core down with it and then made the liveness watchdog blame the waiter. The wedge detectors
exist because a silent hang is the one failure this project refuses above all others. Removing them
re-opens 26.6 violations that were closed deliberately.

So the fix is not "spin forever" and not "panic". It is the third option each of these skipped:

1. **Establish that the other core is NOT PROGRESSING, rather than merely late.** `note_irq` already
   stamps a per-core interrupt count. A core whose count is advancing is alive and slow - wait longer.
   A core whose count is frozen across the whole bound is the case these guards were built for. None
   of the five checks it; the kill-path panic does not even print it.
2. **Fail the OPERATION, not the machine.** A kill that cannot reclaim should fail the kill: leave the
   slot un-freed, report it loudly, let the supervisor retry or an operator see a leaked slot. One
   task's frames is a cheap price for the invariant that only the kernel is unkillable. This works for
   the kill path and the allocator; it does NOT work for a TLB shootdown, where a half-completed
   unmap cannot be abandoned - that one needs (1).
3. **Bound by time, not by spins.** Three of the five count iterations. Even with (1) in place, a
   count will fire at a different point on every machine and every emulator.

## What is NOT claimed here

Only `scheduler.rs:2679` has been reproduced. The other four are classified by reading the code and
by sharing its shape, not by observation - and on real hardware a core is not descheduled the way a
vCPU thread is, so several of these may be unreachable outside an emulator. That is a reason to rank
the work, not to leave the class unrecorded: a false panic on a busy CI host is still a machine that
died, and the bar in 22 does not have an "unless the host was busy" clause.

## Next step

The operator's call on scope. `backlog/48`'s instance is the one with a measurement behind it and the
smallest fix (fail the kill). The shared instrument - "is that core's IRQ count moving?" - is what
would retire the whole class, and it is a kernel change, so it belongs on its own branch with its own
re-verification rather than riding another one.
