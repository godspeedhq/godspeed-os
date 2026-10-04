# 72. Pi 4 tasks fault on their own code under `chaos max-carnage`

**Status: OPEN - recorded 2026-10-04. Cause NOT found, but narrowed: the fault-path page walk shows the
table is right and owned by the faulting task alone, so the core used a translation the table does not
hold. One hypothesis (a skipped TLB flush on a recycled page-table root) was tested on hardware and
FALSIFIED; the change it produced is kept because it is strictly more correct.**

## Evidence

Pi 4 (AArch64), `chaos max-carnage all-services 50 yes`, 2026-10-03/04, build `fd55d331` (plus the
context-switch change below for the last run). Four storms:

| Run | Storm reached | User faults with no `PANIC in service` before them |
|---|---|---|
| 1 | round 4 | 2 |
| 2 | round 8 | at least 4 (of 7; three dumps were garbled by serial interleaving) |
| 3, `force_turbo=1` | round 1 | 10 |
| 4, with the context-switch change | round 22 | 8 |

Each storm ends when the SHELL dies, because `chaos` runs as its foreground job. The kernel never panicked,
and the supervisor restarted everything that died (0 `restart FAILED`).

The faults repeat at FIXED addresses, which random corruption would not:

- `ELR 0x44a3c8` = `service_main`, the shell's FIRST instruction - an instruction abort, permission fault
  level 3 (`ESR 0x8200000f`), in runs 2, 3 and 4;
- `0x424960` in `console_write_chunked` (runs 3 and 4), `0x457fb8` in `sleep_ms`, `0x40fdc8` in
  `chaos_launch`;
- `hw-enumerator` "executing" at kernel address `0xffffff800008086c`; `control` and others taking data
  aborts at odd addresses (`0x80000008`, `0x8e002970`).

Each lands within milliseconds of a spawn. The first fault of run 2 came straight after `kill_task ...
'mem-pressure' freed 7236 frames`.

**QEMU does not reproduce it:** a 30-round Pi 4 storm gave 64 and then 50 exceptions, every one the
DESIGNED kind - a service panics on `EndpointDead` and its panic handler faults at address 0 on purpose
(see `project-chaos-service-data-aborts` in the session notes; the tell is the `PANIC in service` line
before it). Only hardware shows the other kind. The August Pi 4 storm (`build/pi4a.log`, `e9a6878d`) had
one such fault in 100 rounds, so the class predates this work; the rate is much higher now.

## It happens WITHOUT chaos (2026-10-04, `f7061574`)

A plain Pi 4 boot, WiFi joined, nothing being killed: at 02:05:53 the shell took the same fault -
`ESR 0x82000007`, instruction abort, translation fault - four seconds after `observe` spawned its live
painter (`observe-live`, core 3). The supervisor restarted the shell in 30 ms. So the storms only raise the
rate; the trigger is not a kill. "Within moments of a spawn" still fits: `observe-live` was the spawn.

A side effect worth naming because it looks like a separate bug: the crashed shell was the one polling `q`
for the live view, so the respawned shell never reaped `observe-live` and it painted on unowned, deaf to
`q`. `kill observe-live` (or starting `observe` again, which kills a stale painter first) clears it; a
respawned shell should reap it itself.

**Ruled out from this capture:** `x22 = 0xd05dead5` (also in run 1's dump) looked like a poison
fingerprint. It is a constant the shell's own code builds (`mov`/`movk` at several sites), so a register
holding it says nothing.

## What is RULED OUT

- **The CPU clock** (run 3). `force_turbo=1` held the Arm clock at 1500 MHz - `power`'s "minimum" read back
  1500 MHz - and the storm faulted MORE.
- **The audio jack's DMA.** Its arena is reserved once and never recycled, `start` resets the channel and
  waits before touching a control block, and every control block's destination is the PWM FIFO: it reads
  its own arena and writes one peripheral register.
- **A stale TLB from a recycled root** (run 4, the falsified hypothesis). `switch_context` installed the
  incoming root, and flushed, only when it differed from the live one; roots are recycled, and a core
  idling on a dead task's root can refill its TLB speculatively from frames being freed. The switch now
  installs and flushes on EVERY switch to a task (`arch/aarch64/context.rs`). The faults continued,
  including `service_main` again. Since the TLB is now flushed before every task runs, a fault's
  translation comes from the faulting task's CURRENT page tables.
- **Writes by the page-table walker.** `TCR_EL1` enables no hardware access-flag updates, and the A72
  cannot do them: a walk reads, never writes.
- **The service-name changes of 2026-10-03.** They touch no page table, and the class predates them.

## What is left

A permission fault on the first instruction of a fresh task, with a flushed TLB, means that task's own L3
entry says "not executable" when the fetch happens. Two explanations survive:

1. **The table's contents are wrong.** A frame holding a live task's page table is also in use by another
   owner - freed by a different task's death, or allocated twice - and that owner's data reads as
   descriptors. Garbage descriptors explain both the permission faults and the wild addresses.
2. **The table is right but published late.** Another core walks it before the stores that built it are
   visible. `finalize_service_address_space` issues `dsb ishst`, but only on the spawning core.

## The walk, on the card (2026-10-04, `5883978b`)

The instrument above shipped as `fault_report` and ran on the Pi 4 the same morning: four shell faults,
one fifteen minutes into an idle WiFi soak (the shell resuming from a `ping` waiting for its reply) and
three in a row on the respawned shell running `selfcheck`. All four read the same:

```text
ESR_EL1 = 0x82000007 (instruction abort, lower EL), translation fault L3
TTBR0 root 0x5af0000 - a live address space
root / L2 table / L3 table / page - allocated - no other address space holds it
L3[86] = 0x20000005db87c7
leaf: EL0 access yes, read-only, EL0 execute yes, access flag set
verdict: the table ALLOWS this access now - the core used a translation the table does not hold
```

- **Explanation (1) is ruled out for these faults.** Every frame on the walk is allocated and held by the
  faulting task alone, and the descriptors are the ones a loader writes, not garbage.
- **But the core faulted at L3 with a TRANSLATION fault** - its walker found that entry invalid - while the
  same entry, read through memory moments later, is valid and executable. The walker read something the
  table does not hold now.
- `TCR_EL1` is right on every core (walks inner write-back, inner shareable), and the kernel writes tables
  through a matching cacheable, inner-shareable direct map. Not an attribute mismatch.
- Each fault is on the first fetch after the shell RESUMES from a blocking call, so just after a switch back
  to it, and the respawned shell got the same root, `0x5af0000`, each time.

Two explanations are left, and they now mean something narrower than (2) above:

- **A stale cached translation.** The core walked with a TLB or walk-cache entry - an upper-level pointer to
  an old L3 table - that the table no longer holds. Every address space uses ASID 0, so an entry cached
  from any of them is usable by all of them.
- **An entry briefly invalid.** Something wrote that descriptor to invalid and back.

## Next step

`at_probe`: at the very top of the trap report, before anything prints, the kernel asks the faulting
core's own MMU to translate `FAR` (`AT S1E0R`), flushes that core's TLB, and asks again. The software walk
reads memory; `AT` reads what the core has cached. Fails-then-succeeds is the stale cached translation;
succeeds at once is the briefly invalid entry. Verified in QEMU (a 15-round storm, the null-page filter
removed for the run: 18 faults, both answers "translation fault at level 2", matching the walk, no panic).
`selfcheck` on the card reproduces the fault within seconds, so one boot answers it.

## Related, latent

ARMv7 (`arch/arm/context_switch.rs`) and x86 (`arch/x86_64/context_switch.rs`) still skip the root install
when it matches the live one. That skip turned out not to be this bug, but it is the same unsound
assumption - equal roots mean the same address space - that recycling breaks. Neither port has a sighting:
the Pi 2's storms and three 50-round T630 and Wyse storms the same night ran clean. Changing x86 touches the
context-switch path, so CLAUDE.md 20 asks for B10 and B1 before and after.
