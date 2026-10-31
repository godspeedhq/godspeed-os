# 49 - five wedge detectors halt the machine on LATENESS, and cannot tell it from a fault

**Opened:** 2026-09-24
**Status:** OPEN - one instance PROVEN (`backlog/fixed/48`) and fixed, the QEMU respawn wedge found to be a deadlock on the x86 clock lock and fixed (2026-10-31, below), the TLB-shootdown bound deleted with its
shootdown, and the spinlock and frame-allocator bounds moved from spins to time (2026-10-28, `99095070`); the two
scheduler bounds remain, neither reads the stuck core's interrupt count, and the lock change was carded for cost on the T630 on 2026-10-31 (no cost it could pin, row below).
*(2026-10-09: the PROVEN instance is fixed - `backlog/fixed/48` closed 2026-09-25 by deleting the kill-path
panic. The other four were not re-checked for this note.)*
**Raised by:** the operator, ruling on `backlog/fixed/48`: *"Nothing above the kernel should have influence
to panic or wedge the kernel. Nothing. If the kernel panics it'll be because of the logic inside the
kernel itself."*

## The rule is already written, twice

This is not a new policy. `CLAUDE.md` 22 states it in the strongest language the document uses:

> Fuzz tests find the inputs that crash the kernel that no one would write by hand. The bar is
> **binary and absolute**: the kernel must never panic on user-controllable input.

> Bar: every attack returns a defined error. Any attack that succeeds is a security hole; any attack
> that **panics the kernel is a kernel bug**. Both are **mandatory fixes**.

`backlog/fixed/48` was recorded as though the remedy were a design choice for the operator. It is not. The
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
| `task/scheduler.rs:2679` | `liveness_deadline_cycles()/4` = 75 quanta ~ 0.75 s | another core releasing a task slot - **PROVEN reachable, `backlog/fixed/48`** |
| `task/scheduler.rs:1616` | 300 quanta ~ 3 s of ticks | another core making any progress |
| `smp/ipi.rs` (gone) | 500,000,000 **spins** | other cores acknowledging a TLB shootdown - **deleted 2026-10-10**: the shootdown it bounded had no caller, and both went (`backlog/fixed/80` K20) |
| `smp/spinlock.rs` | ~~spins~~ **FIXED 2026-10-28**: the watchdog's allowance in counter ticks, and only when the SAME hold lasted it (each lock counts its releases beside its held flag, so a waiter sees contention as the count changing and opens another window, at most 4). Perf under WSL2 + KVM, ten runs before and six after, interleaved: every median inside the before runs' spread; B2 cross-core p50 the one leaning slower (46 of 60 pairings, not significant at 5%) and the instrument's own floor ~10%, so a T630 perf card was owed before the cost is called nil. **Carded 2026-10-31:** the T630, four boots of `osdev image --mode perf` at `ad3252ac`, two with the change reverted and two without, nothing else different. B2 cross-core p50 96,755 and 107,389 reverted, 111,633 and 104,482 with it: one inside the reverted spread, one 4% above it. B1, B3 and B9 identical to 1%; B4 and B7 lean slower (B7 3,129 and 3,168 against 3,195 and 3,498), B8 leans faster. Two boots a side cannot reach significance (the best ordering is 1 in 6), and the code agrees with no cost: an uncontended acquire is the one compare-and-swap it was, and a release adds a plain load and store beside the flag | a lock holder releasing |
| `memory/allocator.rs` | ~~spins~~ **FIXED 2026-10-28**, the same `smp::spinlock::Wait` | the frame-allocator lock holder releasing - its old message named "preempted holder" as a cause |

Three of the five were bounded by a SPIN COUNT (none is now: the shootdown's went with it, and the spinlock and
allocator are bounded by time since 2026-10-28), which is the shape this project has already learned
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
3. **Bound by time, not by spins.** Three of the five counted iterations (done 2026-10-28 for the two still standing). Even with (1) in place, a
   count will fire at a different point on every machine and every emulator.

## What is NOT claimed here

Only `scheduler.rs:2679` has been reproduced. The other four are classified by reading the code and
by sharing its shape, not by observation - and on real hardware a core is not descheduled the way a
vCPU thread is, so several of these may be unreachable outside an emulator. That is a reason to rank
the work, not to leave the class unrecorded: a false panic on a busy CI host is still a machine that
died, and the bar in 22 does not have an "unless the host was busy" clause.

## Seen again: the liveness watchdog, in QEMU, at 1.002x its allowance (2026-10-18)

`osdev test shell`, during `chaos kill-storm supervisor 4`, as the kernel respawned the supervisor a fourth time:

```text
KERNEL PANIC: panicked at kernel\src\task\scheduler.rs:1871:21:
LIVENESS WEDGE: core 0 made NO progress for 7783025157 counter ticks (1x the 7769852100 allowed); it was
running task slot 9 'shell'; since its last stamp it has taken 0 timer interrupts and halted 0 ...
```

0.17% over the allowance, on x86 QEMU (TCG), on a branch whose only changes since the last clean run were inside
`net-stack` (feat/packages, the TCP receive window and a read-time poll). The same code then ran the whole suite
again, 276 passed, no panic. That is this entry's shape exactly - a wedge detector stopping the machine on a core
that was late by a margin a busy host explains - and the first time the table's "another core making any progress"
row (`task/scheduler.rs:1616` when the table was written, line 1871 now) has been seen firing outside riscv64
(`backlog/fixed/16`).
Recorded, not fixed: the serial log is not kept in the tree.

**And again, 2026-10-19, the same test and the same line**, on `feat/gs-driver` (the `gs::driver` grants,
AHCI and GENET on them, and net-stack's subnet mask - all userspace): `7787690401` ticks against
`7770659700` allowed, 1.0022x, core 0 running `shell`, during the second of four supervisor kills. Twenty
checks after it then failed because the machine had halted, so one late vCPU read as 21 failures. The
rerun, on the same build: 276 passed, 0 failed, 2 skipped, and no panic.

**And three more times, 2026-10-27, on `feat/v1-groundwork`** (the backlog/90 D2 and D5 kernel fixes), two
of them back to back on one build: `7789450287` ticks against `7773654000`, 1.002x, core 0 running `shell`,
the second supervisor kill. Every sighting, including the three logs kept from 2026-10-21 and 2026-10-26
before any of these fixes existed, stops on the same line - right after `spawn[ipc]: 'supervisor' takes back
a reply mailbox released by a service that died - 69 of 96 routing slots free, reserve 72` - so the late
core is inside the supervisor's respawn SPAWN, and it took 0 timer interrupts for the whole allowance:
interrupts were off on core 0 for about 7.8 s while the kernel loaded the supervisor's image (which embeds
every other service's). That is a fact this entry did not have, and it moves the question from "the detector
is too strict" to "why is a spawn running that long with interrupts off". The same build's other suites all
passed (reply-dead 5, fs-restart 11, file-cap 16, identity 24, property 10, adv 15, chaos 8), and the D2-only
build ran this suite clean (276/0) once. Not fixed; the switch-path work this entry gates is the place.

**A third time, 2026-10-21, the same test, the same line and the same shape**, on `feat/gs-driver` (the shell's
history load, the console's blink and comment corrections - all userspace): `7785467269` ticks against
`7769721900` allowed, 1.0020x, core 0 running `shell`, at the second of four supervisor kills, and 21 checks
failed behind it (255 passed). Three sightings, each at 1.002x and each in this one test, suggest that a
supervisor respawn here comes close to the allowance whenever it fires on this host, so a small delay is enough;
not measured. The rerun on the same build: 276 passed, 0 failed, 2 skipped, no panic. The serial log is kept
with the session, not in the tree.

**A fourth time, 2026-10-24, the same line and the same shape in a different test**: `osdev test script`, in
`selfcheck`'s `chaos kill-storm supervisor 1`, on `feat/driver-packages` (`ask`'s radio rows and the supervisor's
`ask-net` peers, on top of the morning's USB-host, `net-stack` and `download` changes - all userspace).
`7825082374` ticks against `7799560200` allowed, 1.0033x, core 0 running `shell`, 0 timer interrupts and 0 halts
since its last stamp, as the supervisor was respawned (`respawning (#1)`). Eight checks failed behind it (3
passed). The rerun on the same build: 11 passed, 0 failed, no panic. So it is not one test's timing: it is a
supervisor respawn on this host, whichever test asks for one. The serial log was overwritten by the rerun; the
panic line is quoted here as it was read.

**A fifth time, 2026-10-26, and then tipped on purpose-free changes.** `osdev test shell`, on `feat/v1-groundwork`,
core 0 running `shell`, 0 timer interrupts and 0 halts, at a supervisor respawn of `chaos kill-storm supervisor`,
each time 21 checks failing behind it (255 passed), at 1.0002x to 1.0024x the allowance. It came with a scheduler
guard for `backlog/95` that NEVER ran its slow path, and went with it: wedged in 5 of 7 runs with the guard (as a
function called on every switch, and as an inline compare with the function only on a mismatch), 0 of 3 without.
The guard was reverted. So on this host the respawn sits close enough to the allowance that a few instructions on
the switch path can decide whether it passes - which makes this entry the gate on judging ANY change to the
switch path with `osdev test shell` (`backlog/95`).

## Measured, 2026-10-28: the respawn is NOT the 7.8 s with interrupts off

The paragraph above (2026-10-27) concluded that "interrupts were off on core 0 for about 7.8 s while the kernel
loaded the supervisor's image". Measured, that is wrong. Every supervisor respawn now logs how long it took, in the
watchdog's own units, and how many interrupts core 0 took meanwhile (`poll_supervisor_respawn`, `kernel: supervisor
respawn #N took ...`). In `osdev test shell` on x86 QEMU, eight respawns:

```text
kernel: supervisor respawn #1 took 152058794 counter ticks (19/1000 of the watchdog's 7775053200), core 0 took 6 interrupt(s) meanwhile
kernel: supervisor respawn #2 took 93412450 counter ticks (12/1000 of the watchdog's 7775053200), core 0 took 4 interrupt(s) meanwhile
...
kernel: supervisor respawn #8 took 110751678 counter ticks (14/1000 of the watchdog's 7775053200), core 0 took 4 interrupt(s) meanwhile
```

About 40 ms each, 1.2% to 2.1% of the allowance, with core 0 taking its tick throughout. So a respawn that runs to
completion is nowhere near the watchdog, and the wedges are something else. Two facts every sighting shares now read
differently:

- **1.002x is when the watchdog first LOOKS, not how late the core was.** Another core checks on its own tick, so
  a core that stopped for good is reported at just over 1x. Every QEMU sighting at 1.0002x to 1.0033x is therefore
  consistent with a core that NEVER came back - a real stall - and not with one that was slightly late. The
  "lateness" reading this entry was opened on does not fit them.
- **Core 0 was running `shell`, not the spawn, with 0 interrupts and 0 halts.** The spawn is preemptible and
  round-robins with ready tasks (`timer_tick_from_irq`), so after "takes back a reply mailbox" the shell had the
  core, and from then on core 0 took no interrupt at all: either interrupts stayed masked (a kernel path on the
  shell's behalf that spins with IF=0) or its one-shot TSC-deadline timer was never re-armed. Which one is not
  known; the next step is to make the wedge report say whether the stalled core's timer was armed.

The kill path's cross-core wait (`backlog/fixed/48`) is not it: it skips the calling core and is bounded.

**Once more, 2026-10-29, the same shape** (`osdev test shell`, 255 passed, behind it): at the supervisor's second
respawn, right after "takes back a reply mailbox", core 0 running `shell` (slot 9), 1.0006x the allowance, 0 timer
interrupts and 0 halts since its last stamp. On a tree carrying `backlog/90` D28 (the interrupt mask and unmask
`asm!` no longer `nomem`), which touches exactly the masking this question turns on, so the run was repeated: three
more passed 276/0 with no wedge. One in four with the change, none in the three earlier runs that day without it,
and the earlier sightings predate it - not a difference this many runs can show either way, and recorded here so
the next count includes it.

## Found, 2026-10-31: the QEMU sightings were a DEADLOCK on the x86 clock lock, not lateness

The measurement above said the wedged core "NEVER came back", with interrupts masked or its timer unarmed. It
was the first: core 0 spinning with interrupts off on a lock whose holder it had itself preempted.

Caught in `osdev test shell` (run 5 of 6 at `ad3252ac`; 255 passed, 21 failed), with two panics at once:

```text
SpinLock WEDGE: lock@0xffffffff81b90f58 held by ONE holder for 7778300888 counter ticks (1x the watchdog's
7772322900) with no release - a deadlock or a dead holder, not contention
LIVENESS WEDGE: core 0 made NO progress ... running task slot 9 'shell'; ... 0 timer interrupts
```

The lock is `arch::x86_64::rtc::CMOS`, added 2026-10-18 (`80f2bb49`) so two cores could not interleave the
0x70/0x71 pair - and 2026-10-18 is this entry's first x86 sighting. Taken with plain `lock()`, on the reasoning
that no interrupt handler reads the clock. But the supervisor respawn runs on core 0 where the timer can preempt
it (the round-robin above), and it reads the clock in `commit_task`. Preempted holding `CMOS`, it handed core 0 to
`shell`, whose clock syscall - interrupts off, as every syscall runs - spun on the lock with no tick left that
could give the core back. Every sighting's shape follows: core 0 "running `shell`", 0 interrupts, right after
"takes back a reply mailbox" (the respawn in flight), reported at just over 1x because that is when the watchdog
first looks.

**Proven both ways with `cmos-hold-test`** (the respawn holds `CMOS` for a twentieth of the allowance, so a tick
lands inside the hold): with the plain lock, the same two panics on the FIRST respawn, 255/21; with `lock_irq()`,
276/0, core 0 taking 6 to 12 interrupts through each respawn. **FIXED 2026-10-31:** `CMOS.lock_irq()`. It is the
log ring's bug again - `log::write_fmt` records the same wedge, found when the respawn was made preemptible, and
masks for it - on a lock added afterwards. The other plain locks on that path were checked: each is either taken
under a mask (`PENDING` under `TABLE`, `KSTACK_USED`, `RING`) or only from contexts that run with interrupts off
(the IOMMU's and PCI configuration's, taken by device spawns and kills; `SEEN` in the syscall path).

**What this does not settle.** The five lateness detectors this entry lists are still detectors of lateness, and
the two scheduler bounds still do not read the stuck core's interrupt count; this sighting was a true stop, which
they reported correctly. The T630's 2026-10-25 wedge (core 0, 0 interrupts, right after a respawn, 1.11x) has the
same shape and may be the same lock; not confirmed.

## On hardware: the T630, twice in one day, each right after a supervisor respawn (2026-10-25)

Both on `feat/driver-packages`, in `chaos max-carnage all-services 1000`, with the WiFi dongle on `xhci`.

**A wedge, 11:38.** As the kernel respawned the supervisor (`respawning (#132)`), three seconds later:

```text
KERNEL PANIC: panicked at kernel\src\task\scheduler.rs:1993:21:
LIVENESS WEDGE: core 0 made NO progress for 6660716855 counter ticks (1x the 5988743100 allowed); it was
running task slot 16 'chaos'; since its last stamp it has taken 0 timer interrupts and halted 0 times in idle
```

1.11x the allowance, not 1.002x, and on real hardware, where no host deschedules the core. Zero timer
interrupts in that window means core 0 was not taking its tick at all, so this one may be a real stall rather
than lateness. Not investigated. The report that followed was interleaved with another core's output on serial.

**A silent reset, 16:12, on `ec7c0e17`.** Round 328 of 1000. Chaos killed the supervisor, the kernel respawned it
(`respawning (#186)`), it adopted the running services and restarted `console`, `control`, `power` and
`audio-driver`, and a fresh `gsh>` was drawn. The last line is `audio-driver` reading its controller's
capabilities (16:11:59.974), just after the kernel confined 00:09.2 and enabled its bus mastering. Then
nothing until the firmware's own output at 16:12:13. No `KERNEL PANIC`, no wedge, no `reboot:` line. The
operator was away and did not reset it. The rerun on the same image ran all 1000 rounds clean, and
`selfcheck` passed 666 of 666 after it.

What can reset this machine without a line, read from the code and not observed:

- **A triple fault.** The x86 TSS sets no Interrupt Stack Table entry (`boot.rs`, `ist: [0; 7]`), so a fault taken
  with a bad kernel stack - an overflow into the guard page - has no stack to report on, the double fault
  fails the same way, and the CPU resets. This is the only silent path in the kernel.
- **`Reboot` (syscall 18) does not flush serial** before `hardware_reset`, unlike `reboot_now`, so its
  `reboot: hardware reset` line can be lost with the reset. Only the shell holds `REBOOT`, and nothing asked it.
- **The platform.** An AMD chipset can reset itself on a fatal device error. The audio controller had DMA
  refused at `0x46a9400` in both runs, 6 and 116 times - an address inside its own arena (its RIRB), so probably
  while its confinement was being taken down or set up, not checked - but it was also restarted 172 and 495 times
  without a reset, so the timing is the only thing pointing there.

Two changes would make the next one say why, and both are kernel changes for their own branch: an IST stack for
the double fault (a triple fault becomes a printed panic), and a serial flush in `Reboot`.

*(2026-10-30: the first is done - the double fault runs on IST1, a 16 KiB stack per core, and reports the likely
stack overflow before halting every core (`backlog/90` D20), so the triple fault above is no longer a silent path.
`Reboot` (18) still resets without flushing serial.)*

## Next step

The operator's call on scope. `backlog/fixed/48`'s instance is the one with a measurement behind it and the
smallest fix (fail the kill). The shared instrument - "is that core's IRQ count moving?" - is what
would retire the whole class, and it is a kernel change, so it belongs on its own branch with its own
re-verification rather than riding another one.
