# 54 - `stack_fit_check` calls itself blind on a binary that genuinely has no frames

**Status:** OPEN - a false positive in a gate, not a missed defect. PRE-EXISTING.
**Found:** 2026-09-26, while building all four ISAs on `feat/stdlib-complete`.

## What it says

```
STACK-FIT CHECK IS BLIND on target x86_64-unknown-none: hello
Every function in those binaries has a zero frame, which no real service has. The
stack-pointer prologue on this target is a form this checker does not match, so it
measured nothing and would have reported a pass.
```

## Why the reasoning is sound and the conclusion is wrong here

The heuristic is a good one and exists for a good reason: a checker that measures nothing and reports
a pass is worse than no checker, and this one refuses to do that. It is the same instinct as the
comment-symbol audit reporting 0 findings while blind.

But `examples/00-hello` is 43 lines. It logs three strings and loops on `yield`. There are no locals
that outlive a call, nothing is spilled, and everything inlines - so it really does contain **zero**
`sub rsp` instructions. Verified with `objdump -d`: the count is 0 across the whole binary.

So "no real service has a zero frame" is true of services and false of a minimal example, and the
gate cannot tell the two apart.

## It was NOT introduced by the stdlib migration

`00-hello` moved from `godspeed-sdk` to `godspeed` in this branch, which is the kind of change that
could plausibly alter codegen. It did not: the pre-migration binary was rebuilt from a stash and has
the same **0** `sub rsp` instructions. The blindness predates the branch.

Worth stating because the first instinct on seeing a gate complain right after a refactor is that the
refactor caused it, and here that would have been wrong.

## Why it matters even though nothing is broken

A gate that cries wolf gets ignored, and this one is guarding something real - a service whose frames
do not fit its 256 KiB stack is a crash on the board with the least headroom. The warning firing on a
binary that is fine trains a reader to scroll past it, which is exactly when it fires on one that is
not.

## The options

1. **A size floor.** Below some symbol count or byte size, a zero-frame result is ordinary rather
   than suspicious. Crude, and it needs a number nobody can justify.
2. **Count the SYMBOLS it measured, not just the frames.** The blindness claim is really "I found
   functions but no prologues". A binary with three functions and no prologues is plausible; one with
   four hundred is not. That distinction is already available and costs one more count.
3. **Leave it.** It is loud, it is honest about what it did not measure, and it names the binary. The
   cost is a warning a reader must learn to recognise.

(2) is the one that keeps the guard's honesty without the false positive.
