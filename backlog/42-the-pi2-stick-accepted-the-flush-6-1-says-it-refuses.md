# 42. The Pi 2's stick ACCEPTED a cache flush, and `CLAUDE.md` §6.1 says it refuses

**Status: OPEN - a constitutional claim and a machine disagree, and the machine has only been asked
once.** Measured 2026-09-22 on the Raspberry Pi 2. Recorded rather than acted on, because §6.1 is a
guarantee in the constitution and one hardware run is not enough to move one (§26.7).

## What §6.1 says

The amendment of 2026-07-25 makes the crash-recovery guarantee **backend-conditional**, and names
this board as the case that cannot be honoured:

> The Pi 2's USB stick refuses `SYNCHRONIZE CACHE` outright, and FUA - which the drive does honour -
> costs more time per write than the driver's command budget can give it. With no barrier available,
> a power cut can lose the tail of a write sequence.

`services/dwc2/src/msc.rs` repeats it at the opcode: *"This board's stick REFUSES it outright"*.

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

## What NOT to do

Do not edit §6.1 on the strength of one green run. A durability guarantee that gets loosened because
a single cut came back clean is exactly the overclaim the carnage document exists to prevent - and
the cut that came back clean was, on this occasion, helped by a ten-second window during which an
idle stick had ample time to flush on its own.
