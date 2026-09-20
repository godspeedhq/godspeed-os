# 23. `recorder` jumped to address 0, and four `selfcheck` assertions passed while it was dead

**Severity:** two items. The crash is a real service fault (non-fatal, the kernel handled it exactly
as it should). The assertions that passed over it are worse, because they are why `ran 461, failed 0`
did not mention any of this.
**Status: item 2 FIXED 2026-09-20; item 1 OPEN.** The four assertions now have a liveness check
in front of them, so the suite can no longer sleep through `recorder` dying. The CRASH itself is
still one unreproduced occurrence with nothing to measure - see the end.

## 1. The crash

Pi 4, `af8dee41`, 2026-09-12 20:35:46, in the middle of `selfcheck`'s `events persist` section:

```
> events persist status | assert contains recording

*** aarch64 EXCEPTION: LowerEL/A64 Synchronous
    ESR_EL1  = 0x82000006  (instruction abort, lower EL)
    fault    = translation fault L2, not a write
    FAR_EL1  = 0x0
    ELR_EL1  = 0x0
    task     = recorder (slot 12)
    x30=0x0   x19..x25=0   x26=0x406030  x27=0xd05dead5
    EL0 fault - killing the task; the kernel and every other service continue.
kill_task: slot=12 'recorder' freed 72 frames
```

**PC = 0 with LR = 0 and every callee-saved register but two zeroed.** `recorder::service_main` is
`-> !` and cannot return, so this is a branch to a null pointer, not a fall-off-the-end. `x27` holds
`0xd05dead5`, which appears nowhere in this repository's source - so it is either a value read from
somewhere unexpected or a leftover in a register the crashing path never set.

The service had already started and was working: `recorder: ready - capturing to 128 KiB` at
20:35:44.441, two seconds earlier.

**It did not recur.** The second boot ran the identical selfcheck step at 20:37:20-20:37:26 and ended
with `recorder: capture ended - 52 line(s), 2819 byte(s), 0 lost to the window`. Same commit, same
image, same card. So it is intermittent, which is the hardest kind to leave unrecorded.

**Ruled out:** not the kernel (it killed the faulting task and everything else continued, which is
invariant 12 working); not a restart cascade (the supervisor did not respawn it - the next
`events persist start` spawned a fresh one at 20:35:46.996). Not obviously the storage path either,
though `recorder` writes through `fs` -> `block-driver` -> `xhci`, and `block-driver` changed in
`af8dee41`; that change is a mechanical `cfg` substitution resolving to what the ISA list resolved to,
so it is a thing to rule out with a second occurrence rather than a suspect with evidence.

**Next step:** the register dump is nearly empty, which is itself the clue - the useful instrument is
a backtrace at the fault, and the kernel's dump already walks the stack and found "none found in
24576 bytes ACTUALLY READ". A `recorder` built with frame pointers, or a poison value written into
x27 deliberately so its origin is known, would name the path. Until it recurs there is nothing to
measure.

## 2. Four assertions passed with the service dead - this is the part to fix

Between the kill at 20:35:46.902 and the fresh spawn at 20:35:46.996, `selfcheck` ran and PASSED:

```
> events persist status | assert contains rotations       assert: ok
> events persist status | assert contains covers          assert: ok
> events persist status | assert contains kib_day         assert: ok
> events persist status | to json | assert contains capacity   assert: ok
```

All four assert on the SHELL's rendering of a status line, and that rendering does not need `recorder`
to be alive. So they are assertions that cannot fail for the reason they appear to be testing, and
`ran 461, failed 0` is a true statement about a suite that slept through a service crash.

This is the `commandments_redteam.py` principle applied to the identity/selfcheck suite: **a check
that has never been observed failing is not yet evidence.** The cheap fix is an assertion that the
status line reports a LIVE recorder (or a `status | where name contains recorder | assert lacks Dead`
alongside, which the suite already does for `hw-enumerator` one section earlier - the pattern exists,
it just was not applied here).

That is a change to the selfcheck script, which is code, so it is recorded here rather than made.


---

## Item 2 fixed 2026-09-20 - the assertions now require a live `recorder`

Exactly the fix this entry specified, using the pattern it pointed at:

```gsh
status | where name contains recorder | assert contains recorder
status | where name contains recorder | assert lacks Dead
events persist status | assert contains rotations
...
```

Placed immediately before the four rendering assertions, so the window they were blind to is the
window that is now checked. `scripts/selfcheck.gsh`, verified by `osdev test script`, which runs the
embedded selfcheck twice in one boot and requires `failed 0`.

**Why this was worth doing rather than leaving recorded.** It is the same defect that was found four
separate ways elsewhere on 2026-09-20, in checks that passed on an empty string, on the echoed
command line, on the words "storage unavailable", and on a marker that the displayed content itself
contained. The general form: **asserting the ABSENCE of a failure, or a string that something other
than the subject also produces, confirms nothing.** Here the "something other" was the shell's own
renderer, which answers whether or not the service it describes is alive.

`ran 461, failed 0` was a true sentence about a suite that had not looked. That is worse than a red
test, because it is a green one.

### Item 1, the crash, is unchanged and stays open

One occurrence, on aarch64, not reproduced on the immediately following identical run. The register
dump is nearly empty (PC = 0, LR = 0, callee-saved registers zeroed except `x26`/`x27`), which is the
clue and also why there is nothing to work with: a branch to null leaves no trail. The next step is
still the one recorded above - frame pointers in `recorder`, or a deliberate poison in `x27` so its
origin is known - and it is not worth building until it recurs.

**What HAS changed is the odds of noticing the recurrence.** Before this fix, a repeat would have
produced the same `failed 0` it produced the first time, and would have been found only by someone
reading the serial log for another reason. Now the run goes red in the section the crash happens in.
That does not fix item 1, but it converts it from a bug that hides into a bug that reports itself,
which is the precondition for ever measuring it.
