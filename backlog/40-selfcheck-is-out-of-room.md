# 40. `selfcheck.gsh` is 376 bytes from a hard ceiling, and the ceiling is a u16

**Status: CLOSED 2026-09-24** (measured 2026-09-22). Not a regression and not new - the file has been creeping
toward this for as long as checks have been added to it. What is new is that somebody finally hit it
and measured the remaining space.

## It has now BITTEN, and the ceiling is platform-dependent (2026-09-23)

The v0.19.0 release build failed on `assert!(SELFCHECK_GS.len() < 65536)`. Nothing was wrong with
the source: the file is **65139 bytes as stored** and clears the ceiling with 397 bytes to spare.

`.gitattributes` had `*.gsh text`, which normalizes to LF in the REPO and converts to NATIVE on
checkout. Native on Windows is CRLF, which adds one byte per line - 1136 lines, so **66275 bytes on
a Windows checkout**. The CI runner is Windows. The same repository builds on Linux and fails on
Windows, and `git diff` reports the working tree clean throughout, because the `text` attribute
normalizes on comparison.

**A file whose SIZE is asserted must have deterministic bytes on every platform.** Fixed with
`*.gsh text eol=lf` (and `*.gs`), the rule `*.sh` and `boot/**` already carry - the latter for the
same class of bug, where a trailing CR became part of a U-Boot filename (`backlog/26`).
`line_ending_check.py` only inspects files under an `eol=lf` rule, so `*.gsh` was invisible to it
and is now covered.

**This does not close this entry - it sharpens it.** The headroom is 397 bytes on LF and was
NEGATIVE on the platform the release is built on. The next section added to `selfcheck.gsh` will
fail the build again, and the failure will be a compile-time assert rather than the silent
wrong-function dispatch it exists to prevent, which is the system working. The room still has to
come from somewhere.

## The number

```
base file          64,830 bytes
ceiling            65,536 bytes   (u16)
headroom              706 bytes   before the job-control section
headroom              376 bytes   after it
```

## Why it is a real ceiling and not a chosen one

`prescan_fns` records each function's body offset in the baked script as a **u16**. A script over
64 KiB wraps those offsets and the interpreter dispatches the WRONG BODY - silently, with no error,
returning another function's result. That is why the build asserts on it
(`services/shell/src/main.rs`, audit U6): failing to compile is the only honest response to a limit
whose violation is invisible at runtime.

So this cannot be raised by editing a constant. It is `u16` -> `u32` through the prescan and every
offset that rides on it.

## What it already cost

The job-control section of `selfcheck.gsh` was written to cover `background` end to end on real
hardware: the refusals, a detached copy, a wait-on-the-truth poll of the job table, and a read-back
of the copied file proving the EFFECT. That is 960 bytes and it does not fit.

What landed instead is 330 bytes: the refusals, and `background drives scrub` with an assertion that
the row appears in `jobs`. It is real coverage - the service spawns, the IPC round-trips, the table
renders, on the actual machine - but it does not verify a job's effect on hardware, and it cannot
clean up after itself, which is why the job had to be a read-only scrub rather than a copy.

The part that did not fit is the part QEMU already covers (`osdev test jobs`, 50/50), so nothing is
unverified. What is lost is hardware verification of the same paths, and this project's own history
is a long argument for why those differ.

## Three ways out, none of them free

1. **Widen the offsets** (`u16` -> `u32` in `prescan_fns` and its callers). The honest fix. It is an
   interpreter change on the path every baked script takes, so it needs its own testing.
2. **Let `selfcheck` call a library script.** `LIBRARY` scripts each get their own budget, but a
   library command is prompt-level only - refused inside another script - because two nested
   interpreter frames overflow the bounded user stack. Lifting that is a stack question, not a size
   one.
3. **Trim `selfcheck.gsh`.** It is heavily commented, deliberately, and those comments are the
   record of why each check exists. Cutting them to buy room trades a permanent explanation for a
   temporary 2 KB.

## Closed 2026-09-24, and the "what not to do" below is why

Nothing was shaved. The suite is nine files under `scripts/selfcheck/`, each under its own copy of the
same ceiling, and the same 509 checks run. `backlog/47` carries the treatment.

The CRLF half of this entry stands as ratified history and its fix is still load-bearing: `*.gsh text
eol=lf` is what makes a size assert mean the same thing on every platform, and it now applies to nine
files instead of one.

## What NOT to do

Do not shave individual checks to fit. That is what this entry exists to prevent: the next person
with something worth testing will quietly write a weaker test instead of a better one, and nothing
will record that the weaker test was a concession to a byte count rather than a judgement about
coverage.
