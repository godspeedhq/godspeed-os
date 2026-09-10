# 16. riscv64 chaos: core 1 takes interrupts and never switches away

**Status:** open. Captured 2026-09-10 at `34e229d0`, `chaos max-carnage`, round 2 of 100.

This is the best data yet on the chaos wedge, and it says something different from what earlier runs
suggested. Recording it before the next attempt so the reading is not re-derived.

## What the machine said

```
KERNEL PANIC: LIVENESS WEDGE: core 1 made NO progress for 40020514 counter ticks
  (1x the 39998000 allowed); it was running task slot 2; it has taken 94726 timer
  interrupts, last vector 0x00000020; detected by core 3.

 h1=8/93628/s8   h2=5/94726   h3=5/76119/s37   h4=5/95404/s21
 idle sample (hart: armed-in/last-seen-ago/stie/stip) now=1984061925
   h1=-38301619/38384132/STIE/no-stip
   h2=-40378897/40458944/STIE/no-stip
   h3=-398424/438420/STIE/no-stip
   h4=-398420/438416/STIE/no-stip
```

## What that rules OUT

- **Not a dead hart.** Core 1 is `h2`, and it has taken **94726 timer interrupts - more than any
  other hart** (h1 93628, h3 76119, h4 95404). The timer is firing and the trap handler is running.
- **Not a lost timer wakeup.** That was the standing suspicion by analogy with x86 audit A8-1 and
  `project_x86_idle_lost_wakeup`. A hart that is being interrupted 94726 times is not asleep on a
  consumed deadline.
- **Not a location that "varies".** Earlier runs recorded stages 1, 5 and 12 and I read that as lock
  contention. Stage 5 is `neutral-sched`, which is stamped BEFORE the switch, so every healthy hart
  also rests at 5 - h3 and h4 are at 5 and are fine. The stage is not discriminating and was never
  evidence for contention.

## What it points AT

Interrupts arrive, the scheduler runs, and **it never switches away from task slot 2**. Two facts
narrow it:

1. **`h2` is the only hart with no syscall stamp.** h1 has `s8`, h3 `s37`, h4 `s21`; h2 has none. So
   whatever slot 2 is doing, it is not making syscalls - it is spinning in userspace, or the stamp is
   being cleared.
2. The watchdog's definition of "progress" is a context switch. A core whose run queue holds exactly
   one runnable task that never blocks would look identical to this, and would be a **false positive** -
   the scheduler correctly re-running the only thing it has. Under chaos, services are killed
   constantly, so a core briefly holding one non-yielding task is plausible.

So the next question is not "why is the hart stuck" but **"what is task slot 2, and was core 1's run
queue empty apart from it?"** Neither is in the dump today.

## A second, separate defect in the same output

**The panic printed TWICE and the two spliced into each other:**

```
kernel: hart stages at halt (stage/irqs) -KERNEL PANIC: panicked at ...
```

Core 3 and core 2 both detected the wedge and both panicked. The `DUMPED` compare-exchange was meant
to leave a single writer on the lock-free serial path, and it did not - the second panic began
mid-line of the first. A panic report that can be interleaved is a panic report that can be
unreadable exactly when it matters most, and this one was only legible by luck.

## Next step

Add to the wedge report: the NAME of the task in the offending slot, and the offending core's run
queue depth. Those two turn "no progress" into either a real wedge or a false positive, and neither
needs a guess. Fix the panic serialisation at the same time, since it is in the same report path.

Reproduce in QEMU first (`scripts/riscv_run.py --cmd`) - `project_riscv64_chaos_wedge` records that
the board was being used as a debugger and should not be again.

---

## ROOT CAUSE FOUND, by reading rather than by booting (2026-09-10)

**Nothing above ring 0 wedges the machine. The kernel's own tick handler blocks on a lock it takes
with interrupts masked, and chaos is only the load that makes the wait long.**

The chain:

1. Core 1 sat at **stage 5, `NEUTRAL_SCHED`** - stamped by `arch/riscv64/mod.rs::timer_tick`
   IMMEDIATELY before `scheduler::timer_tick_from_irq`, with stage 6 (`TICK_DONE`) stamped
   immediately after. So it entered neutral tick code and never returned.
2. `CORE_LAST_TICK_TSC`, the progress stamp the watchdog reads, is written 69 lines INTO
   `timer_tick_from_irq` (scheduler.rs:1434). The first thing that function does is
   `drain_pending_kstack(cid)`.
3. That calls `free_kstack` and `free_page_table_root` -> `memory::allocator::free_frame`, which takes
   **`alloc_lock()`** - inside a trap handler, interrupts masked. The lock's own comment says
   "IRQ-safe: ALLOC_LOCKED is also taken in interrupt context", which prevents same-core re-entry and
   says nothing about a CROSS-CORE holder.
4. Under chaos the deferred-free queue is non-empty on nearly every tick (constant kill + respawn) and
   the allocator is contended by the spawns, so a core can spin there indefinitely.

**THE DISCRIMINATOR WAS IN THE LOG ALL ALONG.** The panic printed twice, 31 ms apart, from two
different cores - and both report **exactly `94726`** timer interrupts for core 1. The count is
FROZEN, not climbing. The core is not taking interrupts at all; it is inside one trap with them
masked. (The first reading of this note said "94726, more than any other hart" and drew the opposite
conclusion. h4 had 95404. The number was never the point - its STABILITY across two reports was.)

## The fix, and it is a pattern this repo already established

`arch/riscv64/mod.rs::ccache_flush_all` carries the argument verbatim:

> Every caller is inside a trap handler with interrupts masked, so a hart spinning for the holder
> cannot be preempted, cannot service an IPI, and cannot be seen to be alive - which is the exact
> shape of the wedge this port is chasing.

That reasoning was applied to the cache controller and given a try-lock. It was not applied to the
allocator lock on the same path. `drain_pending_kstack` should **try** the lock and leave the work
queued for the next tick when it cannot get it: a deferred free that waits one more tick costs
nothing, and a wait that cannot be preempted costs the machine.

Neutral kernel code, so it lands on every port. Reproduce chaos in QEMU first.
