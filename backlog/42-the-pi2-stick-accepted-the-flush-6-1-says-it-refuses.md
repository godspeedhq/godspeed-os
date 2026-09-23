# 42. The Pi 2's stick ACCEPTED a cache flush, and `CLAUDE.md` §6.1 says it refuses

**Status: OPEN - and the machine has now answered BOTH halves of the question. 6.1's Pi 2 example is
contradicted on the evidence; the amendment is drafted below and awaits the operator, because a
constitutional guarantee is not mine to edit.** Measured 2026-09-22 on the Raspberry Pi 2. Recorded rather than acted on, because §6.1 is a
guarantee in the constitution and one hardware run is not enough to move one (§26.7).

## What §6.1 says

The amendment of 2026-07-25 makes the crash-recovery guarantee **backend-conditional**, and names
this board as the case that cannot be honoured:

> The Pi 2's USB stick refuses `SYNCHRONIZE CACHE` outright, and FUA - which the drive does honour -
> costs more time per write than the driver's command budget can give it. With no barrier available,
> a power cut can lose the tail of a write sequence.

`services/dwc2/src/msc.rs` repeated it at the opcode - *"This board's stick REFUSES it outright"* -
and went further, calling the constitution's backend-conditional guarantee something that existed
"precisely because of this device". Corrected 2026-09-23. The CODE was right throughout: it issues
the CDB and reports what the device answers, which is correct whichever way the device goes. Only
the belief around it was wrong, which is the easiest kind of error to leave in place for months.

## What the machine did

A deterministic power cut inside the commit window (`crash-window` build), 2026-09-22:

```
fs: [crash-window] THE JOURNAL IS COMMITTED AND UNAPPLIED - CUT THE POWER NOW (10s)
fs: journal recovered 4 block(s) from an interrupted write
churn verify: 6 file(s) checked, 0 empty, NONE torn
check: the free count already agreed with the tree - nothing was repaired
```

Recovery in its strong form. And crucially, **`fs` printed no durability warning at all**.

That warning is emitted once per mount whenever `block_flush` returns false, and `durable_or_warn`
is called at both commit barriers - so it ran. Its absence means `dwc2` issued a real
`SYNCHRONIZE CACHE(10)` (CDB `0x35`) and the stick **accepted** it. The ordering the journal needs
was enforceable on this run.

**The same stick is used on all three SBCs** (the operator's own words), and it accepts the flush
through `xhci` on the VisionFive as well.

## A second Pi 2 session, with the corrected instrument

2026-09-22, plain image (no crash-window), carrying the fix that separates `Refused` from
`NoAnswer`:

```
run: ran 500, failed 0, skipped 1          (the skip is the PCI this board has not got)
mounts: 2
writes: /tour/a.txt, /sc_fl.txt, /sc/a.txt, the events capture - real transactions
fs: durability NOT attested ...            ZERO occurrences
```

Writes happened, so `durable_or_warn` ran at both commit barriers on both mounts and stayed silent.
The stick attested durability every time. That is three independent sessions with no refusal: this
one, the deterministic-cut run earlier the same day, and the VisionFive through `xhci` - a different
driver, same physical stick.

## A fourth session, on a third controller

Raspberry Pi 4, 2026-09-22, same physical stick: `selfcheck` 509/0/0 twice, a deterministic cut
recovered (`journal recovered 4 block(s)`, `churn verify` 6 files NONE torn, `drives check` 0 bad
and nothing repaired), and again **zero** `durability NOT attested` lines.

So the stick attests durability through all three USB stacks it meets: `dwc2` on the Pi 2, `xhci` on
the Pi 4, and `xhci` on the VisionFive. Four sessions, no refusal, two of them with the corrected
instrument that can tell a refusing device from an absent driver.

## The HP T630 is NOT a fifth data point, deliberately

2026-09-22, HP T630, an unassisted power cut recovered in the strong form with zero
`durability NOT attested` lines. It is recorded in `docs/gsfs-carnage.md` and it is **excluded here
on purpose.**

That board writes to an **AHCI SSD**, not the shared USB stick. 6.1 already states that an AHCI
backend attests durability, so a clean run there confirms the half of the amendment nobody disputes
and says nothing whatever about the device this entry is about. Counting it would be the very error
this entry was opened to correct: reading an outcome as evidence about a device it did not come
from.

The four sessions above are the evidence, and all four are the SAME PHYSICAL STICK through three
different USB stacks. A fifth would have to be that stick again.

## The only local evidence for the refusal is an instrument that could not tell

`build/pi2a.log` (2026-08-30) contains the warning twice. Both times it is immediately preceded by:

```
kill_task: slot=1 'block-driver' freed 72 frames
fs: block-driver did not answer within 30 s (and could not be reacquired) - failing
supervisor: block-driver died, restarting
fs: durability NOT attested by this drive - it accepts no cache flush ...
```

That is a chaos run killing the driver. The flush failed because **nobody answered**, and the
message blamed the drive. `block_flush` returned false for two unrelated facts and the warning
reported only one of them.

**That is fixed** (this entry's commit): the outcome is now `Durable` / `Refused` / `NoAnswer`, and
only `Refused` - the device answering and saying no - is reported as evidence about the drive.

This does NOT prove §6.1 was wrong. The amendment predates that log by five weeks and may rest on a
July run this repository no longer holds. What it does mean is that the instrument a later reader
would reach for cannot support the claim, and the device it names does not refuse today.

## What would settle it

1. **Mount on the Pi 2 and watch for the warning.** It now distinguishes the two cases. A clean
   mount with no warning, repeated, says the stick attests durability.
2. **If it never refuses**, §6.1 needs amending: the guarantee is still backend-conditional - that
   reasoning is sound and `fs-lyingflush` models the unattested case in QEMU - but the Pi 2 stops
   being the example, and the claim becomes about a CLASS of device rather than this board.
3. **If it does refuse on some mounts**, that is more interesting than either: a device that
   sometimes honours a barrier is worse than one that never does, because the guarantee becomes
   conditional on a coin toss and nothing in the system would notice.

## The unassisted cut was RUN, and it came back ambiguous

2026-09-22, Raspberry Pi 2, plain image (no crash-window), power pulled 18 s into `churn 30` with
287 writes behind it. Nobody chose the moment.

```
selfcheck             run: ran 502, failed 0, skipped 1
(cut at 18s)
mount after reboot    fs: mounted GSFS0008 (... 31259033 free)      <- no `journal recovered` line
churn verify          6 file(s) checked, 0 empty, NONE torn
drives check          12 files, 2 dirs, 0 bad; 31259033 free (rebuilt from the tree)
                      the free count already agreed with the tree - nothing was repaired
                      ok - filesystem is consistent
```

**What it settles.** The volume survived an unaimed cut with content intact and accounting exact:
the free count the superblock carried (31259033) is the one a full rebuild from the tree produces.
That is the assertion the stale-superblock bug broke on the VisionFive, holding here on a third
controller. And it is a SIXTH session on this stick with no `durability NOT attested` line, the
third with the instrument that can tell a refusing device from an absent driver.

**What it does NOT settle, which is the thing it was run for.** There is no `journal recovered`
line, so the journal did no work, so the ORDERING the guarantee rests on was never put to the test.
6.1's claim is about what happens when a cut lands INSIDE the commit window; this cut may simply
have missed it.

**And the build could not tell us which.** `Fs::recover` had four early returns and only the first
was genuinely uneventful. Past the magic check a record EXISTS, and both remaining bails were
silent - so "no journal recovered" covered two different facts: no record was found, or a record was
found with a failed CRC and correctly discarded. The second is precisely what 6.1 predicts for this
board. Fixed in the same commit as this entry: every bail past the magic check now names what it
found and what it did, and a torn record says outright that the power was lost inside the commit
window rather than outside it.

So the run is not evidence either way about ordering. It is the run that found the reason no run of
its kind could have been.

## THE THIRD UNASSISTED CUT HIT THE WINDOW, AND THE JOURNAL REPLAYED

2026-09-23, Raspberry Pi 2, plain image, power pulled 17 s into `churn 30` at a moment nobody chose.

```
01:53:48  churn: 17s elapsed, 272 writes           <- the cut
01:54:09  GodspeedOS arm32: _start reached SVC     <- the boot AFTER it
01:54:12  fs: journal recovered 4 block(s) from an interrupted write
01:54:12  fs: mounted GSFS0008 (... 31259040 free)
01:55:14  churn verify: 7 file(s) checked, 0 empty, NONE torn
01:55:52  check: 13 files, 2 dirs, 0 bad; 31259040 free (rebuilt from the tree)
01:55:52  check: the free count already agreed with the tree - nothing was repaired
```

Verified rather than assumed: zero crash-window lines in the log, the banner reads `CUT THE POWER AT
ANY POINT` rather than `NOW`, and the recovery line falls after the post-cut boot banner, not before
it. Three attempts is ordinary variance at the rate since measured (3 hits in 5 unassisted
cuts across three boards), not the long-odds run an earlier revision of this entry claimed.

**This answers the half the earlier runs could not.** The commit record was durable BEFORE any home
block moved - that is what a replay of 4 blocks means - so the ordering the journal rests on was
enforced on this device. Two clean cuts measured durability; this one measured ORDERING, which is
what 6.1 is actually about.

## The case for amending 6.1

Both halves of the Pi 2 sentence are now contradicted by the machine:

| 6.1 says | the machine says |
|---|---|
| "its USB stick refuses `SYNCHRONIZE CACHE` outright" | seven sessions, no refusal - three with the instrument that distinguishes a refusing device from an absent driver |
| "with no barrier available, a power cut can lose the tail of a write sequence" | an unassisted cut inside the commit window replayed and recovered in the strong form, nothing repaired |

**What should NOT change.** The guarantee stays backend-conditional. That reasoning is sound, it is
the honest shape of the claim, and `fs-lyingflush` models the unattested case in QEMU. A device that
cannot be ordered genuinely cannot carry the guarantee.

**What should change.** The Pi 2 stops being the worked example, because it is not one. The claim
becomes about a CLASS of device - one that refuses or lies about a flush - rather than about this
board and this stick, and the amendment records that the named example was tested on hardware and
did not hold. That is exactly what this entry's own "What would settle it" called for, point 2,
written before the evidence existed.

The draft is not applied. `CLAUDE.md` is the constitution and 21 requires a recorded rationale for
editing it; the operator sets that, not me. What is recorded here is that the evidence is in.

## CORRECTION: an unassisted cut CANNOT settle this on the Pi 2, and the arithmetic was available

This entry previously called the unassisted cut "the one run that would settle 6.1 either way". That
was wrong, and it sent two plug-pulls after an answer they could not return.

**That claim rested on a number I extrapolated wrongly, and the hardware has since refuted it.**
`services/fs/src/main.rs` says the window *"normally lasts under a millisecond - which is why three
real power cuts on a Dell Wyse produced three clean mounts and not one `journal recovered` line."*
That sentence is about the **Dell Wyse, an AHCI SSD**. I applied it to a USB stick without checking,
computed a per-cut hit probability under 2%, and concluded ~35 attempts were needed for even odds.

**The window IS the checkpoint** - the interval between the commit record becoming durable and the
last home block landing. Those home writes are slow on a USB stick, so on that backend the window is
a LARGE fraction of each transaction, not a sub-millisecond sliver. The two flushes I reasoned about
sit outside it, but they do not shrink it.

Measured, across every unassisted cut on this branch:

| board | unassisted cuts | hits |
|---|---|---|
| HP T630 (AHCI) | 1 | 1 (first attempt) |
| Raspberry Pi 2 (USB/dwc2) | 3 | 1 (third attempt) |
| Raspberry Pi 4 (USB/xhci) | 1 | 1 (first attempt) |
| **total** | **5** | **3** |

Three hits in five. At p = 0.02 that outcome has probability about 1 in 10^4, so the estimate is
refuted rather than merely imprecise. The Pi 2's two misses were ordinary variance, not the
1-in-35 luck the bad number implied - which is also why the third attempt succeeded rather than the
thirty-fifth.

**Recorded because the wrong number was acted on**: it went into this entry, the carnage matrix and
the hardware-pass doc, and it told the operator a test was infeasible when two more plug-pulls would
do. A figure lifted from a comment about different hardware is a guess wearing a citation.

## What the unassisted cuts DID measure, which is not nothing

`journal recovered` was the wrong success criterion, and fixing that makes the runs worth having.

The failure 6.1 warns about is **reordering**: home blocks reaching the medium BEFORE the commit
record, leaving torn metadata with no record to replay from. That does not need the window. It
shows up on ANY cut, as a CRC failure on read or a tree that disagrees with the free count - and
both are checked directly.

| | cut 1 (18 s in) | cut 2 (17 s in) |
|---|---|---|
| mount | clean | clean |
| `journal recovered` | absent | absent |
| `churn verify` | 6 files, 0 empty, NONE torn | - |
| `drives check` | 0 bad; free count exact, nothing repaired | - |
| `durability NOT attested` | absent | absent |

Two unaimed cuts, no reordering damage either time. Each further cut is another independent sample
of the same question and costs about 45 seconds (`churn 30`, cut, reboot, `churn verify`,
`drives check` - no `selfcheck` needed, it has passed twice on this build). Clean samples accumulate
into a real statement about the device even though none of them will reach the journal.

## The instrument that WOULD answer it

Not built, recorded (26.7). A **deliberately short crash window** - tens of milliseconds rather than
ten seconds - makes the hit near-certain while leaving far too little idle time for "the stick
flushed on its own during the pause" to explain a recovery. That is the objection which makes the
existing 10-second window weak evidence about ordering, and shrinking the window is the direct
answer to it.

It is written down rather than built because nothing yet requires it (26.2): the safety question is
being answered by the accumulating clean cuts above, and the ordering question has no consumer today
beyond this entry. If 6.1 is ever to be amended on evidence rather than argument, this is the
instrument that would do it.

## What NOT to do

Do not edit §6.1 on the strength of one green run. A durability guarantee that gets loosened because
a single cut came back clean is exactly the overclaim the carnage document exists to prevent - and
the cut that came back clean was, on this occasion, helped by a ten-second window during which an
idle stick had ample time to flush on its own.
