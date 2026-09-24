# 48 - `chaos max-carnage` panicked the kernel on the kill-path bound, once in four runs

**Opened:** 2026-09-24
**Status:** OPEN - ONE sighting, in QEMU/TCG on a loaded Windows host. Not reproduced since.
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
is empty; the kernel binary is byte-for-byte main's. Nothing in the change under test can have
altered the kill path.

**It is not the guard being wrong to exist.** The message is the guard working exactly as written: a
core that will not release a slot cannot have its stack and page tables freed underneath it, and
waiting forever while pretending otherwise is the silent hang this project refuses. Panicking loudly
is the correct second choice. `backlog/17` records this bound landing as one of the three neutral
kernel changes that closed the riscv64 chaos wedge, hardware-verified on three memory models with
"zero kill-path panics".

## The suspicion, which is a shape this project has hit before

**The bound is a COUNT, not a duration.** `1942859025 counter ticks` of one core's counter is a
different amount of wall time on every machine - and under QEMU TCG on a busy host, a vCPU is a
thread that the host scheduler can simply not run for a long time. A core that is descheduled is
indistinguishable, from the other core's counter, from a core that is stuck.

That is the same class as the count-versus-duration lesson recorded elsewhere in this project, and if
it is the explanation then the fix is not to raise the number: it is to bound the wait by something
that means the same thing everywhere, or to distinguish "this core has not been SCHEDULED" from "this
core is not making PROGRESS". The existing per-core IRQ counters are the obvious instrument - a core
whose interrupt count is still advancing is alive and slow, and one whose is frozen is the case this
guard is actually for.

## What would settle it

1. **Reproduce it.** One sighting in four `osdev test shell` runs on 2026-09-24; three runs before and
   after it were 206/0. Run `chaos max-carnage all-services` in a loop on an idle host, then on a
   deliberately loaded one, and see whether load moves the rate. If it does, the bound is measuring
   the host.
2. **Read the two counters at the moment it fires.** The panic prints `CORE_CURRENT` and
   `CORE_LEAVING` but not whether core 2's timer had ticked at all during the wait. That one number
   separates the two explanations, and it is not in the message.
3. **Only then decide.** If core 2 was genuinely wedged, this is a real kill-path bug and the guard
   caught it. If core 2 was merely not running, the guard is reporting a host artifact as a kernel
   fault, which is its own defect - a false panic teaches an operator to distrust a true one.

## Why this is recorded rather than chased

It surfaced during verification of an unrelated change, it is not reproducible on demand, and the
instrument that would tell the two explanations apart does not exist yet (point 2). Chasing it with
the instruments to hand would be the "name a suspect without an instrument" mistake `backlog/37` cost
four rounds to learn. The serial log is `build/tests/shell_serial.log` at the time of writing; it will
be overwritten by the next run, so the panic text above is the record.
