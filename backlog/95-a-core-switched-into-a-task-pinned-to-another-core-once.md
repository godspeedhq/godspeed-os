# 95. A core switched into a task pinned to another core, once, and how is not established

**Status: FIXED in QEMU, 2026-10-31; carded on the T630, the Wyse, the Pi 4 and the Pi 2 2026-11-01 (the guard never fired, through 128, 116, 120 and 132 supervisor respawns), the VisionFive pending; see "2026-10-31" below.** Was: **OPEN, 2026-10-26.** Seen once in QEMU, not reproduced, cause not established. This is `backlog/90`'s **D11**, which the 2026-10-16 audit found by reading; this entry is the first time it was seen run. A guard was written,
committed and then REVERTED the same day (operator's choice), because with it in the tree `osdev test shell`
wedged at a supervisor respawn 5 times in 7 runs against 0 in 3 without it (below). The race is still in the
kernel.

*Numbered 94 until 2026-10-29, when it was found to share that number with `backlog/fixed/94` (Audit 26);
the commits of 2026-10-26 (`606063b2` and its revert) still call it `backlog/94`.*

## What was seen

Once, in `osdev test driver-packages` on x86 QEMU, on `feat/v1-groundwork` at `3eb00afe`. That branch changes
no kernel code from v0.32.0, so the race is in the released kernel. The suite had passed 36 of 36 earlier the
same day and passed 36 of 36 on the rerun straight after.

```
task: 'godspeed-pm' spawned OK on core 0 (slot 15)
kill_task: slot=15 'godspeed-pm' freed 211 frames
supervisor: SPAWN godspeed-pm core=65535
task: 'godspeed-pm' spawned OK on core 1 (slot 15)
KERNEL PANIC: panicked at kernel\src\invariants\assertions.rs:19:5:
assertion `left == right` failed: invariant violation: task migrated between cores during execution
  left: 1
 right: 0
```

`left` is `TASK_CORE[15]` and `right` the running core: core 0 was in `prepare_ring3_switch` for slot 15,
which slot 15's new occupant, pinned to core 1, now held. The 9.1 assertion is what stopped it; without it
core 0 would have run core 1's task while core 1 could run it too.

## What is known

- **A slot can be reused under a stale pick.** `pick_next` reads a slot's valid flag, state and core as three
  loads, and the claim after it is a CAS on the state alone. A kill does not wait for a core until that core
  PUBLISHES the slot (`core_release_current`), so a task picked but not yet published can be killed, its
  slot freed and reserved by a spawn on another core, and made Ready, and the CAS Ready -> Running then
  succeeds against the new occupant.
- **The window that would need is long.** The reuse is a whole spawn, an ELF load measured in milliseconds,
  so the stale pick must have been held that long. In `run()`'s loop `pick_next` runs with interrupts on and
  `disable_interrupts` comes after it, which is the gap an interrupt could stretch.

## The guard, and why it was reverted

The guard checked, at each of the four switch-in sites and after the claim was published and fenced (when a
kill must wait and the slot cannot be reused again), that the claimed task was pinned to this core, and on a
mismatch gave the claim back through `wake_by_slot` and aborted the switch. It never fired: its log line
appeared in no run, and a build that made its give-back a counting panic never reached it. But `osdev test
shell` wedged with it in, each time `backlog/49`'s liveness wedge at a supervisor respawn:

| Kernel | `shell` runs | Wedged |
|---|---|---|
| Without the guard | 3 | 0 |
| Guard as a function called on every switch | 2 | 2 |
| Guard as an inline compare, the function only on a mismatch | 3 | 2 |
| The same, its give-back a counting panic | 2 | 1 (the wedge, not the panic) |

So a few instructions on the switch path, whose slow branch never ran, went with 5 wedges in 7. The likeliest
reading is `backlog/49`'s: in QEMU a supervisor respawn sits within 0.02-0.3% of the watchdog's allowance, so
a small change to the kernel's code tips it. The sample is small and that is not proven. Reverted rather than
kept because the wedge is the more frequent failure of the two.

## What is ruled out (or did not reproduce)

| Attempt | Result |
|---|---|
| Spin of 200,000 and 20,000 iterations before every claim, interrupts off | too slow to reach the prompt in the suite's 60 s; no verdict |
| Spin of 1,000 before every claim, interrupts off | 36/0, no panic: that window alone is not it |
| Spin of 50,000 in `run()` between `pick_next` and `disable_interrupts`, interrupts on | 36/0, no panic |
| 400 `ask` cycles through `osdev shell` | the driver never saw a prompt on stdout; did not run |
| 300 `ask` cycles on riscv64 `virt`, no disk | `ask` waited on storage and never started; did not run |
| A give-back loop as the cause of the guard's wedges | ruled out: the counting panic never fired |

## 2026-10-31: the guard is back, and the sighting was most likely D10

**Step 1 is done.** `backlog/49`'s QEMU respawn wedge was a deadlock on the x86 clock lock, fixed in `bc823241`.
With it fixed, the guard (`claimed_is_ours`, as in `606063b2`, now with D7's `core_abort_claim` on its abort)
went back in: `osdev test shell` 5 of 5 at 276/0 with no wedge, against 5 wedges in 7 before; identity 24/0,
adv 16/0, chaos 8/0, `driver-packages` 36/0. It fired in none of them. `stale-claim-test` makes the first claim
read as pinned elsewhere, and the task given back still ran (the supervisor came up; shell 276/0).

**The sighting fits `backlog/90` D10 better than the stale pick.** A self-kill released its slot while its core
was still on it. Here: `godspeed-pm` ended on core 0, its slot 15 freed at once, and a spawn on core 1 took it
and made it Ready. Core 0's switch-out then read slot 15 as a live task, and its paths that put the outgoing
task back to Running (the CAS `Ready -> Running` when nothing else is runnable) would claim the new occupant -
a task pinned to core 1 - and the next switch would reach `prepare_ring3_switch` for it on core 0. That is the
panic, in the order the log shows it. D10 is fixed (`cf1b5be9`): the slot is not released until the core has
left it. Not reproduced either way, so this is the likeliest reading, not a proof; the guard stays for the
stale-pick window, which is real by reading whatever caused this sighting.

## The next concrete step (as of 2026-10-26)

1. **Fix `backlog/49` first.** While a supervisor respawn in QEMU sits at the watchdog's limit, no change to
   the switch path can be judged by `osdev test shell`.
2. **Measure the race's rate on the original kernel**: `driver-packages` in a loop until the panic recurs
   (about 10 minutes a run; 1 in 3 runs so far), with each scheduler entry that leaves a slot stale recorded.
3. **Then decide the fix**: holding interrupts off across pick and claim, a generation per slot carried by the
   pick, or the guard again, judged once the suite can tell its cost from the watchdog's margin.
