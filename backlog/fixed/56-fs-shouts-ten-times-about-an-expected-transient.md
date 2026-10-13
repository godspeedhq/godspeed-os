# 56 - `fs` announces an EXPECTED transient ten times at every boot

**Status:** CLOSED 2026-10-11 (`feat/ask`) - the ten lines were `fs`'s own boot protocol selftest, not a
retry; they are counted in its PASS line now. The diagnosis below was wrong, and the last section says how.
**Found:** 2026-09-26, on the Dell Wyse and the Raspberry Pi 2, during the `feat/stdlib-complete`
hardware pass. Not new, and not caused by that branch.

## What it looks like

Ten identical lines in ~550 ms, on every boot, on both boards:

```
fs: flash REFUSED - block-driver reports 0 capacity (no disk, or the driver is mid-revival). Retry once storage settles.
fs: flash REFUSED - block-driver reports 0 capacity (no disk, or the driver is mid-revival). Retry once storage settles.
...  (x10)
fs: mounted GSFS0008 (62533296 blocks, bitmap 1..15332, root@15342, 62517833 free)
```

Then it mounts and everything works. The Pi 2 does the same over its USB stick, and mounts.

## Why the behaviour is RIGHT

`fs` asks `block-driver` for a capacity before it will format or mount. At boot the driver has not
finished IDENTIFY yet (AHCI on the Wyse, BOT/SCSI over DWC2 on the Pi 2), so it truthfully answers 0.
`fs` refuses to act on that rather than guessing, says why, and retries. The message even names both
causes and tells the reader what to do. That is invariant 12 and 26.7 working exactly as written - the
alternative, silently treating "0 capacity" as "no disk", is the silent fallback the constitution
forbids.

The line is also genuinely load-bearing in the other case: if there really is no disk, this is the only
thing that says so.

## Why it is still worth fixing

**A warning printed ten times for an expected condition is a warning that gets scrolled past.** Every
boot on every board trains the reader that `flash REFUSED` is noise. The one boot where it means "your
stick is not plugged in" reads identically to the nine hundred where it meant "wait 300 ms".

This is the same argument `backlog/fixed/54` makes about `stack_fit_check` crying wolf on `00-hello`, and
the same one the 2026-09-23 amendment in CLAUDE.md 6.1 makes about an instrument that could not tell a
refusing device from a dead driver. A correct message at the wrong volume is an instrument problem.

## What it is NOT

Not a mount failure, not a durability problem, and not related to the flush question that CLAUDE.md
6.1 turns on - that is `SYNCHRONIZE CACHE`, a different call on a different path. No durability warning
appeared in any of these sessions. Nothing here is a regression: the retry loop, the message and the
mount are all long-standing.

## The options

1. **Say it once, then once more if it persists past a threshold.** A latch, the way the SDK's
   metric-name truncation warning already does it (`TRUNCATED_SAID`) - one line per service per boot
   rather than one per attempt. The first line keeps the diagnosis; the repetition adds nothing.
2. **Distinguish the WAIT from the VERDICT.** The first N refusals are an expected settle, so log them
   at a lower volume (or not at all) and print one loud line only when the retries are exhausted -
   which is the case that actually means "no disk".
3. **Wait on the truth instead of retrying on a timer** (Commandment VIII). `block-driver` knows when
   IDENTIFY completes; `fs` currently discovers it by asking repeatedly. If the driver announced
   readiness, `fs` would ask once and the message would only ever appear when it was true.

(3) is the honest fix and the largest. (1) is a latch and closes the erosion today. They are not
exclusive - (1) now, (3) when the block path is next opened.

## Closed (2026-10-11) - it was never a retry

The explanation above - `fs` asking `block-driver` repeatedly while IDENTIFY finishes - was a plausible
story and it was wrong. Logging the raw request in the flash arm on a Pi 4 boot showed every one of the
ten carried tag 0 and one of the shapes `protocol_selftest` builds:

```
DEBUG56 flash tag 0 p [21]
DEBUG56 flash tag 0 p [149]
DEBUG56 flash tag 0 p [21, 255]
DEBUG56 flash tag 0 p [21, 4, 47, 97, 98, 99]   (the "/abc" probes)
```

That selftest walks every opcode through `serve_once` with `capacity = 0`, and the zero is what stops it
formatting the real disk (the comment on `serve_once` records the boot on which it did not). Each flash
probe was refused on that zero and logged the refusal exactly as a real "no disk" would - ten flash and
ten `drives-info` lines on every boot, on every board, indistinguishable from the case the line exists
for. Nothing was formatting anything and no retry loop exists.

So the options above do not apply. `serve_once` takes a `quiet` flag that only the selftest sets, the
three lines it was producing are gated on it, and the selftest says what it did once:

```
fs: protocol selftest PASS - 599 malformed requests, every one answered (10 of them flash requests, each
refused on the zero capacity the test injects - no disk was touched)
```

A real `drives flash` with no disk still logs the refusal, and a disk that never reports a capacity still
logs `block-driver did not report capacity within 20s` - the line that means "no disk". Pi 4 QEMU: 20
lines to 1. `osdev test files` 249/0 with zero `flash REFUSED` lines in its log.
