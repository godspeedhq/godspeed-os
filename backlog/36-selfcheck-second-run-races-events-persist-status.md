# 36. `selfcheck`'s second run intermittently fails on `events persist status`

**Status: OPEN, and INTERMITTENT - which is the whole reason it is written down.**

Seen once on 2026-09-18 during unrelated work (the console scrollback branch), on
`osdev test script`:

```
--- failures ---
FAIL  events persist status | assert contains recording
run: ran 492, failed 1
```

An immediate re-run of the same suite, same build, gave `5 passed, 0 failed`. So it is a
race, not a regression: the case passes far more often than it fails.

## Why this entry exists rather than a shrug

`osdev test script` runs `selfcheck` **twice in the same boot** specifically to pin
re-runnability, and its own comment records that this exact case has broken that property
before:

> once when `delete /sc` on an already-clean tree was the single failure in an otherwise
> perfect run, and again when `events persist status` asserted "not running" - true only
> before `recorder` had ever been spawned, so a second run said "idle" and failed. Hardware
> found that one, on a Pi 4, after the first two runs had passed.

That second one was fixed. What is left is narrower and different in kind: the assertion
does not report the WRONG state, it reports a state that has not arrived YET. `events
persist start` asks `recorder` to begin, and `events persist status` is asked immediately
afterwards; on a loaded host the reply can be composed before the recorder has transitioned.

**A suite that only passes on a fresh boot fails the first time somebody runs it twice,
which is exactly when they are investigating something.** A suite that passes four times out
of five is worse: it teaches people to re-run rather than to read, and the next real failure
gets re-run too.

## What closing it looks like

Not a sleep. The fix is the one this project keeps reaching for (Commandment VIII - wait on
truth, not on time): `events persist start` should not return until `recorder` has actually
started recording, or `status` should report the requested state alongside the observed one
so the difference is visible rather than a race. Either makes the assertion deterministic.

Until then: a lone `events persist status` failure on a SECOND `selfcheck` in one boot is
this, and a re-run is the right diagnostic - but only once, and only for this case. Any
other failure, or this one twice, is something else.
