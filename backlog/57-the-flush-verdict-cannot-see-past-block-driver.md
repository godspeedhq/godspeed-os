# 57 - `fs` reports "the drive REFUSED the flush" when the drive was never reached

**Status:** OPEN - an instrument defect, reproduced on two boards and two ISAs. It produces false
evidence for a CONSTITUTIONAL claim (CLAUDE.md 6.1), which is why it is filed rather than noted.
**Found:** 2026-09-26, during the `feat/stdlib-complete` five-board pass, by following up a warning
that looked like the evidence 6.1 asks for.

## What it says, and why that sentence matters more than most

```
fs: durability NOT attested - this drive ANSWERED and refused the cache flush, so journal write
ordering is unenforced and a power loss may leave metadata torn.
```

CLAUDE.md 6.1's 2026-09-23 amendment turns on exactly this message. It retracted a two-month-old
claim that the Pi 2's stick refuses `SYNCHRONIZE CACHE`, on the grounds that the only evidence had
been *an instrument that could not tell a refusing device from a dead driver*. It closed with:

> No board in this project is currently known to refuse the barrier. **If one is found, it belongs
> here with the evidence that put it here.**

So this message is the designated evidence for amending the constitution. It fired six times in one
afternoon. It was wrong all six times.

## The measurement

Chaos `max-carnage 1000` on both boards. Every warning, with its line in the session log:

| Board | ISA | `Refused` | `NoAnswer` | Outside chaos |
|---|---|---|---|---|
| Pi 4 | aarch64 | 4 (66804, 76198, 119014, 150095) | 4 | **0** |
| VisionFive 2 | riscv64 | 2 (117099, 124254) | 2 | **0** |

**All six `Refused` lines have an unreachable `xhci` immediately before them.** 6 of 6, two ISAs, two
different physical setups. The Pi 4 pattern:

```
block-driver: 'xhci' did not answer, and the retry after reacquire COULD NOT REACQUIRE - no live instance
block-driver: the USB host service did not ANSWER (restarting, or its cap went stale) - reporting storage UNAVAILABLE, not 'no disk'
fs: durability NOT attested - this drive ANSWERED and refused the cache flush
```

and the VisionFive's, with the kill visible in the same millisecond:

```
kill_task: slot=2 'xhci' freed 385 frames
block-driver: 'xhci' did not answer, and the retry after reacquire COULD NOT REACQUIRE - no live instance
fs: durability NOT attested - this drive ANSWERED and refused the cache flush
```

The drive was not reached. No flush was issued to any device. Nothing was learned about either stick.

## The actual defect: a fourth state, modelled as one of three

The 2026-09-23 amendment fixed this class at the `fs` <-> `block-driver` boundary, replacing a bool
with `Durable` / `Refused` / `NoAnswer`. That was right and it works. The break is **one layer
deeper**, at `block-driver` <-> `xhci`, where nothing distinguishes:

| State | What it means | Modelled? |
|---|---|---|
| `Durable` | the flush reached the device and succeeded | yes |
| `Refused` | **the DEVICE declined** | yes |
| `NoAnswer` | `block-driver` never replied | yes |
| - | `block-driver` replied, but its OWN dependency was dead, so nothing reached the device | **no** |

`block-driver` is alive and answers promptly, so `fs` correctly observes "answered" - and then reads
that answer as the device's verdict. It is `block-driver`'s verdict about a device it could not talk
to. The honest fourth value is `Unreachable`, and it belongs to `block-driver` to report, because it
is the only component that knows its own dependency died.

`block-driver` ALREADY knows: it prints `reporting storage UNAVAILABLE, not 'no disk'` two lines
earlier, which is precisely this distinction, drawn correctly, for a different question. It just is
not carried into the flush reply.

## Why this is worse than an ordinary wrong log line

**It manufactures evidence for a claim nobody can check cheaply.** Whether a device honours a cache
flush is not something a reader can verify by inspection; they have to trust the instrument. That is
why 6.1 nominates this message as the evidence, and why a false positive here does not just mislead,
it launders. Had this pass reported "the Pi 4 and the VisionFive both refuse the barrier, amend 6.1",
the amendment would have looked well-evidenced - six sightings, two boards, two ISAs - and would have
repeated the exact error of the original Pi 2 claim with more apparent rigour behind it.

It is also the second recurrence of one root cause. Same shape as the Pi 2 story: a real distinction,
a component that knows it, and a reply that flattens it before it reaches the component that reports.

## What this does NOT say

- **Not a failure of this branch.** `fs`, `block-driver` and `xhci` are byte-identical to main here.
- **Not a mount or data failure.** Both boards ran `selfcheck 516/0` before and after, and `fs`
  re-mounted cleanly every time (`mounted GSFS0008 ... 31259060 free`).
- **Not evidence that either stick DOES honour the flush.** It is evidence that these six lines say
  nothing either way. The seven-session silence 6.1 relies on is unaffected; these are not
  counter-examples to it, they are noise that looks like counter-examples.
- **Not an argument for softening the message.** When it is true it is exactly right.

## The fix

1. **Carry `Unreachable` in the flush reply.** `block-driver` distinguishes "the device said no" from
   "I could not ask" - it already prints the difference - so the reply gains a fourth value and `fs`
   reports what it was told. Small, local, and it makes the message trustworthy for the claim 6.1
   rests on.
2. **Then re-run the measurement.** A `Refused` surviving chaos after (1) is real, and is the evidence
   6.1 asks for. Until (1) lands, a `Refused` seen during chaos means nothing and should not be cited.
3. **Cheapest interim, if (1) waits:** name the suspicion in the message itself - "if the host
   controller restarted, this verdict is unreliable" - so a reader is not invited to trust it.

(1) is the fix. It is a service-level change on the block path, not a kernel one.

## The lesson worth keeping

The amendment that fixed this class wrote down the rule correctly and then checked only the boundary
it was looking at. A conflation fixed at one layer reappears at the next one down, because the reason
it happened - a reply that drops a distinction the sender knew - is a shape, not a location. When a
message is nominated as the evidence for a constitutional claim, the interesting question is not "is
it wired up" but "what else can make it fire".
