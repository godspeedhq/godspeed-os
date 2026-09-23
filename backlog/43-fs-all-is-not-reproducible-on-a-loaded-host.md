# 43. `fs-all` is green, and not REPRODUCIBLY green on a loaded host

**Status: OPEN, mechanism measured.** Two full sweeps on 2026-09-23 both returned 31 of 33, and
**they failed different suites each time.** Every failing suite passes when re-run alone. That is the
signature `backlog/30` warns about - "dismissed as host load each time, which is exactly how a real
intermittent bug stays invisible" - so this one is written down with the measurement that entry
lacked.

## What happened

| sweep | result | failed | cause |
|---|---|---|---|
| 1 (~62 min) | 31/33 | `jobs`, `fs-unplug` | REAL harness defects, both fixed (`efc9b8b4`) |
| 2 (~74 min) | 31/33 | `fs-tear-detect`, `fs-blockdeath` | host starvation, below |

Sweep 1's failures were genuine bugs and are gone. Sweep 2's are a different thing, and the giveaway
is the clock rather than the assertion:

| suite | in the sweep | run alone |
|---|---|---|
| `fs-tear-detect` | **573 s**, 0 passed 8 failed | 31 s, 8 passed 0 failed |
| `fs-blockdeath` | **234 s**, 7 passed 1 failed | 35 s, 11 passed 0 failed |

Eighteen times and seven times slower. A suite does not get eighteen times slower because it found a
bug.

## The mechanism, measured rather than assumed

`build/tests/fs_tear_detect_serial.log` from the failing run:

```
block-driver: op 5 spent 16939 us in the driver (slow #960)
fs: op 10 took 200089 us, 18 block ops, 99% of it inside them
block-driver: op 5 spent 6290 us in the driver (slow #1088)
```

**One write took 200 milliseconds, and 99% of that was inside block operations.** Individual driver
ops took 6-17 ms against a normal figure in the tens of microseconds, and over a thousand were
flagged slow in a single run. The guest booted and mounted normally; it was the emulated disk
crawling.

Not disk space (279 GB free) and not a stray QEMU (checked, zero). Ambient host I/O contention -
`build/tests/` holds 2.1 GB across 353 files and the sweep writes hundreds of images through it,
which on Windows is also exactly what an on-access virus scanner notices.

## Why this is a real finding and not an excuse

**A green `fs-all` is a merge gate, and a gate that only passes on an idle machine is not a gate.**
Someone will run this on a laptop doing something else, get 31/33, and have to decide whether the
filesystem is broken. Today that decision took reading two serial logs and re-running two suites.

The deeper form is Commandment VIII - **wait on the truth, not a clock.** A suite that fails because
its deadline expired while the work was still progressing is asserting on elapsed time, and elapsed
time is a different quantity on every machine (`a count is not a duration`, and neither is a
timeout). The per-op deadlines in `fs` are correct and deliberate - they bound a real wait. The test
harness inheriting them as pass/fail criteria is what makes the RESULT machine-dependent.

## What would close it

1. **Name the starvation rather than failing as a defect.** The data is already on the serial -
   `fs` prints `op N took X us, 99% of it inside them` and `block-driver` counts its own slow ops. A
   suite that sees those should report STARVED, distinct from FAILED, the way `fs-unplug` now
   refuses a wrong-machine kernel instead of booting into silence.
2. **Then decide per suite** whether the assertion is about latency at all. Most are not: they are
   about outcomes that remain true however long the work takes.

Recorded rather than fixed (26.7): the diagnosis is one run old, both suites are green when run
alone, and guessing at a fix before knowing which assertions are genuinely time-based would be the
speculative work 26.2 forbids.

## Do NOT

Do not re-run until green and record that. Two sweeps, different failures, both explainable is not
the same as a passing gate, and a tally that only counts the run you liked is how `backlog/30`
started.
