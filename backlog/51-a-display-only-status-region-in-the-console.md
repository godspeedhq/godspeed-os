# 51 - a display-only status region in the console, for any long-running command

**Opened:** 2026-09-24
**Status:** OPEN - designed, not built. Deliberately not started before the branch closes.
**Raised by:** the operator, after three attempts to give `selfcheck` a progress display failed for
the same reason each time.

## What is being asked for

A **limited, temporary console region** that only DISPLAYS. Nothing can be typed into it and no
command can be invoked from it - a few reserved rows where a long-running command shows a bar, a
count, a "what am I doing now", while ordinary output keeps scrolling above.

Not for `selfcheck`. For anything that runs long enough that a person wonders whether it is stuck.

## Why the three previous attempts failed, because the design follows from it

`selfcheck` got a progress line per part. It scrolls past in a second among 516 other lines, so it
answers nothing at the moment somebody looks up.

It then got a painted full-screen frame, twice, named `view` and then nearly named `live`. Both names
were rejected and both rejections were right: **they were modifiers hunting for a noun.** A mode of
`selfcheck` is the wrong unit. The thing wanted is a FACILITY, and a facility needs no word at the
command - `selfcheck` would simply use it, the way it uses the screen today.

And the painted frame had a defect no name would have fixed: `ConsoleWrite` (syscall 23) writes
**serial AND the TV**. Every repaint therefore lands in the serial transcript - the artifact the
harness greps and every hardware result is read from. A display that costs the evidence trail is not
a display worth having.

## Where it belongs: the `console` service

It already owns the grid, the cursor, `scroll()` and the scrollback history (`services/console/src/term.rs`).
A reserved region is a property of a grid. So this is inside the service's existing responsibility,
**adds no kernel responsibility, and needs no kernel change** (Commandment I untouched).

Two things fall out of putting it there rather than in the shell:

**The serial problem disappears.** The console already answers IPC requests (`REQ_DIMS`,
`REQ_HISTORY`, dispatched on a single-byte opcode in `services/console/src/main.rs`). A status region
set over THAT path renders to the framebuffer only. Serial never sees a repaint. This is the reason
the facility cannot live in the shell: the shell's only route to the screen is the syscall that also
writes serial.

**No scroll region is needed.** The console implements `A B C D f G H J K h l m` and no `r`/DECSTBM -
and does not need it. If the console owns the reservation internally, `scroll()` scrolls rows
`0..N-k` instead of `0..N`. That is a change inside the function that already does the scrolling,
not a new terminal feature.

## The shape

Two new opcodes on the existing console protocol (2 and 4 upward are free; the dispatch already drops
unknown opcodes loudly, so an older console meets a newer caller safely):

```
REQ_STATUS_CLAIM   rows            -> a region handle, or a refusal
REQ_STATUS_SET     handle, text    -> draw it; framebuffer only
REQ_STATUS_RELEASE handle          -> the rows go back to the scroll
```

`SET` carries the whole region's content, not a delta: the console holds no model of what the caller
meant, and a caller that dies mid-update leaves no half-line.

## The question to settle BEFORE building it

**Who owns the region, and what happens when that owner dies?** A command holding four rows that is
then killed must not leave the screen permanently short. Two answers:

1. **A delegated resource capability (7.10).** The console mints a region cap, the holder draws
   through it, the console revokes on death - exactly the mechanism behind file-as-capability and
   socket-as-capability. It fits the architecture rather than sitting beside it, and revocation is
   already solved.
2. **An opcode with the owner tracked by endpoint.** Cheaper, but then the console has to notice
   deaths, which it currently has no reason to do.

(1) is almost certainly right, and it is the part worth thinking about properly rather than
discovering halfway through.

## The rate bound, which is not optional

`scroll()` is exactly where this project's repaint lesson lives: painting per message jammed the
console's 16-deep queue and ate the shell's own keystroke echo - the keyboard looked dead and was
not. A status region updated per statement is that mistake again. The update rate must be bounded by
the DESIGN - the console coalescing, or a minimum interval it enforces - not by every caller
remembering to be careful.

## What it is NOT

**No input.** That is a security property, not a simplification: no input path means no new
authority and no confused deputy. A surface that can only be drawn into.

**Not a window system.** One region, at the bottom, N rows. The moment it grows a second region or a
z-order it has become something else and should be argued for on its own terms.

## Who would use it

`selfcheck` (516 checks, nine parts), `churn` (thousands of transactions), `chaos` (round counts),
`drives check` and `drives scrub` (whole-volume sweeps), recursive `copy`, `fs-model`. All of them
today either print into the scroll where it vanishes, or show nothing at all. That is the 26.2 pull:
several real callers, none speculative.

## Why it is not being built now

It changes the terminal that every port renders through, on a branch that is one machine from done
and validates none of it. `feat/stdlib` closes first.
