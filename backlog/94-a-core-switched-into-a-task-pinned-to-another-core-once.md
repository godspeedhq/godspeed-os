# 94. A core switched into a task pinned to another core, once, and how is not established

**Status: OPEN, 2026-10-26.** A guard is committed (`claimed_is_ours` in `kernel/src/task/scheduler.rs`), so
the machine no longer halts if it recurs, but the race has not been reproduced and its path is not known.
Recorded at the operator's request ("Commit the guard, record the gap").

## What was seen

Once, in `osdev test driver-packages` on x86 QEMU, on `feat/v1-groundwork` at
`3eb00afe`. That branch changes no kernel code from v0.32.0, so the race is in the released kernel. The suite
had passed 36 of 36 earlier the same day and passed 36 of 36 on the rerun straight after.

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
- **What the guard does.** After the claim is published and fenced, when a kill must wait for this core and
  the slot cannot be reused again, it checks the claimed task's core. A mismatch gives the claim back
  (`wake_by_slot`) and aborts the switch as a lost kill race does. It says so on the first occurrence and
  every power of two after. It changes nothing on any switch where the core matches.

## What is ruled out (or did not reproduce)

| Attempt | Result |
|---|---|
| Spin of 200,000 and 20,000 iterations before every claim, interrupts off | too slow to reach the prompt in the suite's 60 s; no verdict |
| Spin of 1,000 before every claim, interrupts off | 36/0, no panic: that window alone is not it |
| Spin of 50,000 in `run()` between `pick_next` and `disable_interrupts`, interrupts on | 36/0, no panic |
| 400 `ask` cycles through `osdev shell` | the driver never saw a prompt on stdout; did not run |
| 300 `ask` cycles on riscv64 `virt`, no disk | `ask` waited on storage and never started; did not run |

So neither widened window reproduced it, and reading on, an idle core's tick may not switch into a task
the way the `run()` theory needs. The path by which core 0 held slot 15 stale is open.

## The next concrete step

1. **Wait for the guard to speak.** Its line, `sched: core N claimed slot S, reused for a task pinned to core
   M`, is the evidence this entry needs; any run that prints it pins the path down to that moment.
2. **Measure the rate on the original kernel.** `driver-packages` in a loop until the panic recurs (about
   10 minutes a run; 1 in 3 runs so far), with each scheduler entry that leaves a slot stale recorded.
3. **Then decide the root fix** - holding interrupts off across pick and claim, or giving each slot a
   generation the pick carries - with the guard kept as the backstop either way.
