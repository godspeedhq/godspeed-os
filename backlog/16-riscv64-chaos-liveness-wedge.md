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
