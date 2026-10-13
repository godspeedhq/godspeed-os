# 36. `selfcheck`'s second run intermittently fails on `events persist status`

**Status: CLOSED 2026-09-20. It was a fixed SLEEP racing a variable pre-fill, and the cause
is not the one this entry guessed - see the post-mortem at the end.**

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


---

## Closed 2026-09-20 - and this entry had the wrong cause

The guess above was that `status` "reports a state that has not arrived YET" because the recorder
had not transitioned. **It had.** `cap.on = true` is set BEFORE the `START` reply is sent, and
`persist_begin` waits for that reply and checks its op tag, so by the time `start` returns the
recorder genuinely is on. Fixing the recorder would have changed nothing.

The state the probe wanted had not been REACHED, which is a different thing. `build_persist_status_table`
has three live states, not two:

```rust
} else if on && filled < capbytes {
    b"preparing"        // on, but the extent is still being pre-filled
} else if on {
    b"recording"
}
```

`preparing` is real and deliberate - the extent is allocated up front and made readable in the
recorder's own loop, so the caller is never blocked on device I/O. The suite then did:

```gsh
wait 3
events persist status | assert contains recording
```

A fixed three seconds against a variable amount of disk work, under a comment that said "wait for
readiness rather than assuming it" - which is exactly what a sleep does not do. On a loaded host the
pre-fill had not finished, the status still read `preparing`, and the assertion failed. An immediate
re-run passed, which is the signature this entry correctly identified as the dangerous part: it
teaches people to re-run rather than read.

**The fix is the one this entry prescribed**, applied to the right place: poll the real state,
bounded at 30 attempts, and fail loudly and specifically if it never arrives. Staged through a file
because gsh refuses to capture a pipeline, and `count` gives an unambiguous 1-or-0 where reading the
raw table would see a header row either way.

Two things worth keeping. **A wrong entry is still worth having** - it recorded the symptom, the
frequency and the correct principle, which is what made the real cause findable; only its mechanism
was wrong, and that was fixable by reading the code it named. And **the third state is the tell**: a
probe that knows two states of a three-state system will fail on the third exactly as often as that
state occurs, which is the same shape as `fs-tear`'s probe not recognising one of its own oracle's
two legal answers, found the same day.
