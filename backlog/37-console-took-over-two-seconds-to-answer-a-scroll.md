# 37. The console took over two seconds to answer a scroll request, and I do not know why

**Status: the PERMANENT failure is SOLVED - a stale capability, and a regression I introduced
while fixing the first symptom. The original two-second stall is still unexplained.**

> **Update 2026-09-19.** The instrument added for this answered it on the first run, and the answer
> was not what any of the reasoning below predicted.
>
> ```
> 03:35:32.049  run: ran 492, failed 0
> 03:35:32.049  gsh> console: scrolled back 30 of 512 lines - the screen is showing HISTORY
> 03:35:39.456  cap::get: ResourceId(102) gen mismatch cap=3 rec=29 liveness=Alive
> 03:36:05.231  call: slot 8 waited 1000218 us across 3 blocks (slow #3)
> 03:36:05.231  scrollback: the console stopped answering - left the view
> 03:36:06.253  scrollback: the console did not answer - not scrolling      (then once a second,
> 03:36:09.575  scrollback: the console did not answer - not scrolling       indefinitely)
> ```
>
> **The shell was holding a dead handle.** Generation 3 against a kernel record at 29, endpoint
> `Alive`: the console had been replaced twenty-six times since that cap was minted, which is what
> `selfcheck` does to services. The shell caches peer handles and it is the client's job to
> reacquire (CLAUDE.md §14.3).
>
> **And it was permanent because of a change made in this very backlog entry's fix.** The retry was
> removed from `console_scroll` on the argument that it exists to survive a peer RESTART and "the
> next `console_dims` reacquires for everyone". Nothing in the scroll path reacquires, so once the
> cap went stale every scroll failed forever. The reasoning was wrong and hardware falsified it in
> one run.
>
> **The instrument earned its place by what it did NOT print.** There is not one
> `console: pass N took NNN ms` line in the whole log. That was the discriminator: the console was
> never stuck in a pass, so it was reachable and serving - the fault was entirely on the shell's
> side. Three rounds of reasoning had pointed at the console.
>
> **Fixed** by reacquiring once on entry to the scrollback view, where it costs a kernel directory
> lookup rather than a round trip, and leaves every keystroke inside the view at a single request.
> Still no retry inside `console_scroll`: a keystroke must never cost two deadlines.
>
> **STILL OPEN: the original two seconds.** A stale cap should fail fast, not consume the whole
> deadline, and both logs show the full deadline elapsing (2,000,457 us then 1,000,218 us) with
> three blocks. Why a send on a stale handle waits out its deadline instead of returning
> `EndpointDead` promptly is not established, and is now the only part of this entry that is
> unexplained. It is bounded and reported, so it costs one second and says so - but the mechanism
> is unknown.

## What happened

Dell Wyse 5070, 2026-09-18, immediately after `selfcheck` finished (492 statements, `failed 0`):

```
16:16:57.303  gsh> console: scrolled back 30 of 512 lines - the screen is showing HISTORY
16:17:02.921  console: returned to live (output arrived)
16:17:04.921  call: slot 8 waited 2000457 us across 3 blocks, 1450871 core halts (slow #3)
```

Slot 8 is the shell. It blocked for **2,000,457 us** - the deadline, to the microsecond - waiting
for a reply from `console`. The operator saw the machine lock up and eventually come back.

## What was fixed, and what that does NOT include

Two defects, both real and both now closed:

1. **A timed-out scroll was read as "we are at live."** `console_scroll` returned `(0, 0)` for both
   "the view is at the bottom" and "I could not reach the console". The shell left the scrollback
   view believing it had scrolled back, returned to the prompt, and **the screen stayed in history**
   with nothing said - which is why the exit above logged `(output arrived)` rather than
   `(requested)`. It returns `Option` now, and the failure is reported.
2. **A keystroke could block the shell for four seconds** (a 2 s deadline plus a reacquire-and-retry).
   A scroll is now 1 s with no retry: nothing above the kernel may hang on a peer, and a scroll that
   cannot be served quickly must fail quickly and say so.

**Neither of those explains the two seconds.** They turn a mystery lockup into a reported failure,
which is the right behaviour either way - but if the console really cannot answer for two seconds
after a heavy run, scrollback will now FAIL loudly at exactly the moment somebody wants it, which is
better than hanging and still not good.

## What has been ruled out

- **Not a slow paint.** The console reports any repaint over 250 ms and reported none. Its own
  measured cost on this display is 29-41 ms.
- ~~**Not a stale send cap.** `selfcheck` neither kills nor restarts `console` (checked), so the
  shell's cached slot was not invalidated mid-run. Scrolls had worked forty seconds earlier in the
  same boot.~~ **WRONG, and wrong in an instructive way.** It IS a stale cap: `cap=3 rec=29`. The
  check that produced this line looked for `kill console` / `restart console` in `selfcheck.gsh`
  and found none - but `selfcheck` drives `chaos`, which restarts services without naming them in
  the script, and a generation 26 ahead is not subtle evidence. "Scrolls worked forty seconds
  earlier" was consistent with the cap going stale in between, which is exactly what happened. A
  ruled-out list is only as good as the question each entry actually asked.
- **Not the drain loop by design.** The console drains at most a 16-deep queue, bounded by an
  adaptive paint deadline clamped to 100 ms, then paints. One cycle is ~105 ms at this display's
  cost. A request should wait at most one cycle.

## What to do next

The instrument for this is already in place and cost nothing: **the kernel's slow-call diagnostic is
what found it**, and the shell now prints `scrollback: the console stopped answering - left the view`
at the exact moment. The next hardware run therefore discriminates without new code:

- **The message appears** and the kernel reports a slow call -> the console genuinely is not
  answering, and the next question is what it is doing. Add a timestamp to the console when a
  request is DEQUEUED versus when it is served, which separates "queued behind work" from "slow to
  serve".
- **Neither appears** -> it was a one-off tied to that boot, and the fixes above have removed the
  consequence.

Reproducing it in QEMU has not been attempted and may not be possible: the whole setting is a 4K
framebuffer whose paint cost is an order of magnitude above QEMU's, right after several hundred
statements of output.

**Written down rather than closed** (26.7) because the honest state is "the symptom is handled and
the cause is unknown", and a backlog entry saying so is worth more than a commit message implying
the latter was the former.
