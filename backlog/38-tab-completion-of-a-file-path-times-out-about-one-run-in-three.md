# 38. Tab completion of a file path times out about one run in three, and takes the next case with it

**Severity:** test reliability, and it has been taxing every storage session. Nothing is known to be
broken in the shipped shell; what is measured is a suite that goes red about a third of the time for
a reason that is not the thing under test.
**Status: OPEN, MEASURED 2026-09-20.** Bisected far enough to exonerate the console; not root-caused.

## The signature, which is always the same two lines

```
files-test: PASS - dir (cwd) shows both files
files-test: FAIL - tab abs-path timeout
files-test: FAIL - tab: relative n<Tab> completes to note.txt + runs
files-test: PASS - mkdir relative -> /docs/sub
```

One timeout, exactly one cascade, then it recovers. The case is a single line with a 10 second
budget (`osdev/src/shell_test.rs`):

```rust
match run!(b"read /docs/i\t\r", 10) {
    Some(r) => check!(r.contains("nested-content"), "tab: /docs/i<Tab> completes to inside.txt + runs"),
    None    => { println!("files-test: FAIL - tab abs-path timeout"); fail += 1; }
}
```

**The second failure is not a second fault.** It reports as a content mismatch rather than a
timeout, because the harness reads the previous case's unconsumed output: the `\r` on a completion
that never arrived runs something other than what the test meant, and the stream is one reply out of
step until the following case resynchronises. So the count is `2 failed` for one event, and on a bad
day it has been `6 failed` for one event.

## The rate, measured rather than felt

| commit | date | runs | reproduced |
|---|---|---|---|
| HEAD (`bbd24478`) | 2026-09-20 | 3 | 1 (`242 passed, 3 failed`) |
| `66569aad` | 2026-09-18 | 4 | 1 (`220 passed, 2 failed`) |

Plus two sightings recorded in commit messages at the time: `46f3b00c` (2026-09-19, `239/2`) and
`bbd24478` (2026-09-20, `239/6`, noted there as "the FOURTH time today").

So roughly **one run in three to one in four**, stable across two commits two days apart. That is far
too high to be host load, which is the explanation it has been given until now.

## What the bisect settled, and what it did not

The suspicion was the console: `scrollback` gave the console a history ring on 2026-09-18 and a
deletion of the scroll-request mechanism landed on 2026-09-20, and the flake was noticed in that
window. **Both are exonerated.**

`66569aad` is two hours BEFORE the console gained any of that (`f27c5e10`, 2026-09-18 13:40), and the
flake reproduces there. And the failing case itself is older than either:

```
3a853fdc  2026-06-20 16:45  shell: Tab-completion for file paths (numbered menu + Tab-cycle + common-prefix)
```

So the upper bound on this bug's age is **three months**, not two days, and every console commit is
outside the window. What the bisect did NOT do is find the introducing commit: one reproduction takes
four runs on average at ~6 minutes each, so each bisect point costs roughly an hour to call
"present" and considerably more to call "absent" with any confidence. That is why this is recorded
here rather than continued.

## Why a longer timeout is the wrong fix

The budget is already 10 seconds for a completion that should take milliseconds. Raising it would
convert a visible flake into a slow suite that still occasionally fails, and would discard the one
number that makes this measurable. A wait that is wrong is not a wait that is too short.

## What is worth doing first, in order

1. **Stop the cascade. DONE 2026-09-20.** One event now costs one failure. The `run!` macro in the
   files suite resynchronises after a timeout - two prompts, 5 s each - so the next case no longer
   reads the last one's leftovers. It is reached ONLY on the failure path, so a green run does not
   execute a line of it, and `osdev test files` was 245/0 with it in place.

   This does not make the flake rarer. It makes the count mean something: a sighting that said
   `3 failed` or `6 failed` should now say `1 failed`, and if it still says more than one, that is
   new information rather than the noise it has been.
2. **Then measure the completion path.** Completing `/docs/i` requires the shell to list `/docs`
   through `fs`, so the suspects are the ones that path already has a history with: a reply matched
   to the wrong request, or a stale reply left in the mailbox by an earlier caller (`7c9a74db`
   fixed one such case and this survived it). The instrument that would settle it is a timestamped
   log of the completion request and its reply, which does not exist yet.

### The same trap is in two other suites

`fs-restart` and `file-cap` define the identical `run!` macro with the identical no-resync
behaviour. Neither has been observed flaking, so neither was changed: altering a path with no
evidence of a fault is how a fix becomes a regression. Recorded here so that when one of them does
report an odd multi-failure, the first question is already written down.

**Not ruled out and not yet suspected on evidence:** anything about `/docs` specifically, the
contents of the directory at that point in the suite, or the interaction with the preceding `cd`.
Naming a suspect without an instrument is what cost four rounds on `backlog/37`.
