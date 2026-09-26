# 56 - `fs` announces an EXPECTED transient ten times at every boot

**Status:** OPEN - correct behaviour, wrong volume. Cosmetic, but it erodes a real warning channel.
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

This is the same argument `backlog/54` makes about `stack_fit_check` crying wolf on `00-hello`, and
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
