# 94. VisionFive: a core takes timer interrupts, about 1,450 a second, and never reaches the tick

**Status: OPEN - seen once, 2026-10-22, on the VisionFive 2 Lite at `6bfd508d`; not investigated.**
Recorded at the operator's choice, as its own entry rather than under `backlog/49`, because the evidence says
the core was stuck rather than late.

## What the machine said

During `drives check`, about 40 s after three 10 MB downloads over the cable at 1.4-1.7 MiB/s, with `copier`
spawned on core 2 to run the check:

```
KERNEL PANIC: panicked at kernel\src\task\scheduler.rs:1993:21:
LIVENESS WEDGE: core 1 made NO progress for 40020673 counter ticks (1x the 39998000 allowed); it was running
IDLE (no task) 0 ''; since its last stamp it has taken 14465 timer interrupts and halted 14368 times in idle
(89118 timer interrupts since boot, last vector 0x00000001); detected by core 0. No forward progress = loud stop.
kernel: hart stages at halt (stage/irqs) - h1=15/34485/s37@0x0000000040216dd8 h2=16/89128/s50@0x0000000040216d5a
  h3=15/186644@0x0000000040216dd8 h4=15/36707/s35@0x0000000040216dd8
  idle sample (hart: armed-in/last-seen-ago/stie/stip) now=662540885 h1=-474137/514134/STIE/no-stip
  h2=-265561/305558/STIE/no-stip h3=-474206/514353/STIE/no-stip h4=-471461/511458/STIE/no-stip
```

The log is `build/serial_output.log` of that day, from 14:05:57.

## What it says, read against the code

- **The stamp is the scheduler's timer tick** (`CORE_LAST_TICK_TSC`, `task/scheduler.rs`). Core 1 took 14,465
  timer interrupts since its last stamp - about 1,450 a second over the 10 s window, against the 100 a tick
  would be - and halted in idle 14,368 times, so it was taking interrupts and going back to sleep, and the
  path that stamps was not reached from any of them.
- **Not a dead hart and not a lost wake-up**, for the same reason `backlog/fixed/16` gave: a hart interrupted
  that often is not asleep on a consumed deadline.
- **Every hart's deadline is in the past** in the idle sample (`armed-in` negative, by about 0.47 M ticks on
  three and 0.27 M on the fourth), while none shows a pending timer (`no-stip`).

## What it resembles, and what that rules out

`backlog/fixed/16` had the same symptom - "core 1 takes interrupts and never switches away" - and was closed
on 2026-09-11 by `phys_in_ram`'s missing lower bound (`ca543af4`), after which a 100-round `chaos
max-carnage` ran clean. That fix is still in the tree, so this is either a second cause or a path that fix did
not cover. Nothing here is shown yet.

## The next step

Read the RISC-V timer trap path for an interrupt that is taken and returned from without the tick - a
deadline judged not yet due, a re-arm that fires at once, or a stage that returns early - with the stage
numbers above (15 is past-drain, pick_next), then reproduce: the cable downloads followed by `drives check`.
CLAUDE.md 22's rule applies (the kernel must not panic on what runs above it), so this is a fix, not a
tuning of the watchdog.
