# 37. The console took over two seconds to answer a scroll request, and I do not know why

**Status: CLOSED 2026-09-20. The cause was in `term.rs` the whole time and no measurement was going
to find it - see the post-mortem at the end. The mechanism is now DELETED, not merely unused.**

> **2026-09-19, fourth measurement.** The SDK drain was added and the fault reproduced unchanged,
> with **no `sdk: discarded ... abandoned` line anywhere in the log**. So the reply mailbox was
> empty and that was never the mechanism here. The fix stays - `drain_stale_replies` having exactly
> one caller was a genuine latent bug, and the SDK's own comment documents what it costs - but it is
> not this.
>
> **What the same log DOES show, in the same window:**
>
> ```
> net-stack: `time` asked for the clock - no route yet, will retry
> net-stack: a serve pass took 23412 ms (over 1000) - not asking for client requests during it (#1)
> net-stack: a serve pass took 21777 ms (over 1000) - not asking for client requests during it (#2)
> ```
>
> **A service blocking for 21-23 seconds in one serve pass.** That is `backlog/28`, already measured
> and recorded. It is the first thing in any of these logs that is on the right time scale.
>
> **And my diagnostic has been reporting the wrong half.** It prints the CONSOLE's queue. The console
> saying `BlockRecv, queue 0` means it handled whatever it had and went back to waiting - it does NOT
> say the reply reached the shell. If the SHELL's own endpoint is full, the console's `try_send` of
> the reply has nowhere to land, the console never errors, and the caller waits out its deadline for
> a message that was never deliverable. A stalled peer backing unrelated traffic up into that queue
> is exactly the shape `backlog/28` would produce.
>
> The shell now reports `our queue N` alongside. That is the number that has been missing for four
> rounds of this.

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
> `EndpointDead` promptly is not established.

> **RETRACTION, same day, before the next flash.** The update above claims the stale cap was the
> cause. **It is not established, and two things in the same log contradict it.**
>
> 1. **The `gen mismatch` lines are at 03:35:24, 03:35:26 and 03:35:39 - during and just after
>    `selfcheck`, not in the failure window.** `selfcheck` deliberately exercises stale caps; that
>    is part of what it tests. There is **no `cap::` line at all** between 03:36:06 and 03:36:20
>    while every scroll was failing, so the capability was VALID and the sends were accepted.
> 2. **Every failure took almost exactly 1.000 s** (03:36:14.015, :15.025, :16.018, :17.027, ...).
>    That is the deadline elapsing. A cap the kernel rejects fails immediately and does not consume
>    a deadline at all - so the message was delivered and no reply came.
>
> **And the instrument cannot see the case that matters.** The console reports a long pass at the
> END of the pass. A console stuck INSIDE one never reaches the report, so an absent line means
> either "nothing was slow" or "something was so slow it never finished" - opposite answers. The
> update above read the absence as proof the console was healthy. It proves nothing.
>
> So the honest position is: the console was not answering, the cap was fine, and whether it was
> stuck or never received the message is **still unknown**. The reacquire on entry stays, because
> a client caching a peer handle should reacquire (§14.3) and it costs a kernel lookup - but it is
> hygiene, not a demonstrated fix.
>
> **The next instrument asks the KERNEL, which is the one party that can answer while the console
> cannot.** On a failed scroll the shell now reports the console's task state and queue depth:
>
> - `Running` -> executing, so busy or stuck, and the pass report will confirm which if it ever
>   completes.
> - `BlockRecv`, queue 0 -> idle and waiting, and **our message never arrived** - which points at
>   the send side and away from the console entirely.
> - `BlockRecv`, queue > 0 -> holding the request and not processing it, which should be impossible
>   and would be the most interesting of the three.
>
> Three rounds of reasoning have now produced three wrong answers about this bug. The pattern is
> the one this project keeps relearning: reason to a hypothesis, then MEASURE it, and do not let
> the absence of evidence become evidence of absence.

> **Measurement 2026-09-19, and it narrows this a lot.** The kernel-side diagnostic answered on the
> first run:
>
> ```
> scrollback: the console stopped answering - left the view (console is BlockRecv, queue 0)
> ```
>
> **`BlockRecv, queue 0`.** The console is idle, waiting on `recv`, with an EMPTY queue - not busy,
> not stuck, not slow. And `queue 0` is the strong part: the message was never even ENQUEUED on its
> endpoint. There is no `cap::` line anywhere in the failure window either, so the kernel did not
> reject the send.
>
> A send that the kernel accepts, does not error on, and never delivers, while the intended receiver
> sits idle with an empty queue, is consistent with one thing: **it is going somewhere else** - a
> handle naming an endpoint that exists, is alive, and nobody reads.
>
> **It does not reproduce in QEMU.** A soak that holds the view open for 8 seconds and then scrolls
> passes here (`osdev test shell`), and is kept as a guard in case it ever starts failing. A first
> attempt at that soak DID fail, and it was a harness bug - a bare `ESC` sent alone desyncing the
> escape parser, visible as `[5~` echoed literally. A reproduction that cannot be told apart from a
> test bug is not a reproduction, so it was rewritten to assert the fault's own signature instead.
>
> **Next measurement, which is also a partial recovery.** On a failed scroll the shell now
> reacquires the console by name and retries once, then reports which happened:
>
> - `the console needed a fresh handle - reacquired, carry on` -> the handle WAS the fault, and this
>   is the fix as well as the proof.
> - `... reacquire did not help` -> the handle is fine and the fault is downstream of it, which
>   would point at the kernel's routing or delivery rather than at either service.
>
> Only on the failure path, so a keystroke costs one deadline normally and two only when something
> is already wrong.

> **SOLVED 2026-09-19: the reply mailbox, and it was a latent SDK bug the whole time.**
>
> `reacquire did not help`, which eliminated the handle. That left the reply path - and the SDK
> already documents this failure, by name, with a repair function written for it:
>
> > *"The mismatched reply was discarded and the request failed, which leaves this service's own
> > reply still queued for the NEXT request to find. Every subsequent request then receives its
> > predecessor's reply, discards it, fails, and queues another: permanently one reply out of phase,
> > alternating forever... Observed 133 times in one `osdev test peer-storm` run."*
>
> That is the signature exactly: works repeatedly, ONE request times out, and from that moment every
> request fails for the rest of the boot.
>
> **`drain_stale_replies` had exactly one caller: `fs`.** Every other service was one timeout away
> from being permanently broken, and the shell was the one that got there. It is called from
> `request_with_reply_deadline_into_inner` now, so every caller is repaired rather than every caller
> having to remember - and it is safe for the reason the function's own comment gives: these helpers
> enforce ONE outstanding request at a time, so nothing legitimate can be in the mailbox when a new
> request is issued. Anything there is a reply this service stopped waiting for.
>
> It also LOGS when it drops something, because silent self-repair is how the `fs` version of this
> stayed invisible (§26.7).
>
> **Two of my own diagnostics were misleading and are worth recording as such:**
>
> - `console is BlockRecv, queue 0` is sampled AFTER the timeout, by which point a healthy console
>   that received, served and replied is back at exactly that state. It cannot distinguish "never
>   arrived" from "arrived and was handled". I read it as the former.
> - The console's long-pass report fires at the END of a pass, so a console stuck inside one never
>   reaches it. Absence of that line is not evidence of health.
>
> Both were built to answer this and both could be read two ways. The one that actually discriminated
> was the reacquire-and-retry, because it changed something and reported which way it went.

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

---

## 2026-09-19: a Wyse run that narrows it a lot, and retires one of my own instruments

`selfcheck` (492 statements, 0 failed), then PgUp/PgDn at the prompt. Serial, trimmed to the window:

```
gsh> cap::get: ResourceId(102) gen mismatch cap=3 rec=29 liveness=Alive
console: scrolled back 30 of 512 lines - the screen is showing HISTORY
console: returned to live (requested)
console: scrolled back 30 of 512 lines - the screen is showing HISTORY
console: returned to live (output arrived)
call: slot 8 waited 1000222 us across 3 blocks, 717786 core halts (slow #3)
scrollback: the console stopped answering - left the view (console is BlockRecv, queue 0; our queue 0; reacquire did not help)
scrollback: the console did not answer - not scrolling (console is BlockRecv, queue 0; our queue 0)
```

### What this rules OUT, including a diagnosis of mine

**It is not a stale cap at entry, and the gen-mismatch line is not the trigger.** That line is at the
prompt, and **three scrolls succeed after it**. Scrolling works, repeatedly, and then stops. Any
explanation that starts "the shell's console cap went stale" has to account for the three that
worked; none does. This is the fourth time a stale cap has looked like the answer here.

**The `our queue 0` in those lines is not evidence, and I put it there.** It was added specifically
to answer "did the reply arrive and sit unread". It cannot: it reads
`task_stat(shell).queue_depth`, the task's OWN endpoint, while `request_with_reply*` waits on the
**reply mailbox** (`reply_mailbox()`), a separate endpoint `task_stat` has no query for. So it
reported a zero about an endpoint the reply was never going to arrive on. Relabelled in the code
rather than deleted, because it still answers a narrower question honestly.

### What the run does establish

The mailbox IS measured, by something better than a depth. `drain_stale_replies` runs before every
request and logs loudly when it finds anything. **Across the entire failing run it never fired**, so
the mailbox was empty. Combined with the console's side:

1. **The console never received the request.** `BlockRecv, queue 0` is a service idle at its recv.
   And `serve_request` replies to `REQ_SCROLL` on every path - there is no branch where it takes one
   and stays silent - so had it arrived, a reply would exist.
2. **No reply was ever sent.** The mailbox was empty every time (1), and the shell's own endpoint
   was empty too.
3. **But the send was ACCEPTED.** `call_deadline_into` returning `Err` makes
   `request_with_reply_deadline_into_inner` return `None` immediately; the observed wait is the full
   `1000222 us` deadline, three times over, one second apart. A rejected send cannot produce that.

**(3) against (1) is the contradiction to chase.** A send the kernel accepted, for a message that is
not in the target's queue, to a service sitting at `recv` that never woke. That is a statement about
delivery or wakeup, not about the console's code or the shell's.

### The trigger is now specific

Failure begins on the keypress immediately after `console: returned to live (output arrived)` - the
console was scrolled back and something PRINTED, snapping the view to live on its own. The two
preceding transitions are both `(requested)` and both fine. That wording exists precisely to tell
those apart, which is the one instrument here that has earned its keep.

So the shape to reproduce is **output arriving at the console while the view is scrolled back**, not
scrolling as such. That is testable without a 4K framebuffer: scroll back, have another service log,
then scroll again.

### Changed in this commit

- The failure line reports `reacquire ok={true|false}`. "reacquire did not help" was written as
  though the reacquire had succeeded, but nothing checked the return - it covered "the reacquire
  itself failed" equally well, and those are different bugs.
- The helper that produced that number said in its own doc what it could and could not support.
  It has since been DELETED outright, along with the whole shell-side scroll path, when scrollback
  became a utility - so there is no longer a function to point at here.

Still **OPEN**, and still recorded rather than closed (26.7). What is gone is three wrong answers and
one instrument that was manufacturing a zero.

---

## 2026-09-19 (second run): not broken, SLOW - and the deadline never measured the work it bounds

`reacquire ok={true|false}`, added after the previous run, answers the question that had been left
open, and the answer removes the last cap-shaped theory:

```
console: scrolled back 30 of 512 lines - the screen is showing HISTORY
console: returned to live (requested)
console: scrolled back 512 of 512 lines - the screen is showing HISTORY
console: returned to live (requested)
console: scrolled back 512 of 512 lines - the screen is showing HISTORY
console: returned to live (requested)
console: returned to live (output arrived)
call: slot 8 waited 1000209 us across 3 blocks, 718716 core halts (slow #3)
scrollback: the console stopped answering - left the view (console is BlockRecv, queue 0; our queue 0; reacquire ok=true)
```

**`reacquire ok=true`.** The shell reacquired the console by name, successfully, and the very next
call still ran out its deadline. A fresh cap to a live endpoint. That is the fourth stale-cap theory
this entry has retired, and it should be the last: the mechanism is excluded, not merely unlikely.

**SIX scroll operations succeed first.** Whatever this is, it is not present at entry.

### The three facts that fit together

- **`slot 8` is the SHELL** (`task: 'shell' spawned OK on core 0 (slot 8)`). The console is slot 2,
  and its contract pins it to `core = 0`. **The caller and the callee share a core**, on a machine
  that reported `smp: 4 cores ready`.
- **The framebuffer is 3840x2160** (`spawn[fb]: 'console' 3840x2160 ... 32400 KiB`).
- **The 1-second call begins within 9 ms of `returned to live (output arrived)`** - the snap from 512
  lines of history back to live, which is the most expensive repaint this console ever performs.

So the reading is no longer "a message was lost". It is: the console is mid-repaint, the shell gives
it one second, and the shell is blocked on the same core the repaint is running on. 718,716 core
halts in that second is the shape of a caller churning while the thing it waits for holds the core.

**The deadline was picked without measuring what it bounds.** One second for a full 4K repaint from
history, on a shared core, is not a conservative bound - it is a guess that happens to be wrong on
this machine. The prior entries in this file all looked for a lost message because the number was
assumed to be generous.

### Decision: scrollback becomes a utility (operator's call, 2026-09-19)

Not as a way around the bug - the shape is wrong on its own terms, and this bug is what exposed it:

- **One IPC round trip PER KEYPRESS**, each with its own deadline, against a service that repaints
  4K. Holding PgUp issues one request per key repeat. No deadline makes that robust.
- A utility **fetches the history once and pages it locally**: zero IPC per keystroke, and it reuses
  `line_pager` - the pager `paginate` and `help` already share - rather than adding a third.
- It **removes the view offset from the console**. That is a second place holding a derived view of
  where the operator is looking, which is what 26.4 is about, and it is precisely the state that can
  desync from the shell's idea of it.
- `q` to quit matches every other full-screen view (`0_conventions.md` rule 10a).

Honest limits of that change: it still needs ONE successful request to fetch the snapshot, so it
reduces N calls to 1 rather than proving this resolved. And it is a snapshot rather than a live view
- which is the right semantics anyway, since the live screen keeps moving underneath.

**Sequenced after the gsfs carnage gates** at the operator's direction. Until then the current
behaviour is acceptable under 26.7: it fails loudly, says what it observed, leaves the view, and the
session continues. It does not corrupt anything and it does not hang.

Still **OPEN**. What this run cost the entry is one more wrong theory; what it bought is a mechanism
(slow repaint on a shared core) that is measurable rather than speculative - the next step is to
have the console REPORT its repaint cost, so the deadline is derived from a measurement instead of
being chosen and then defended.


---

## Closed 2026-09-20 - the answer was three lines of `scroll_view`

Four measurements, each retracting the last, ending "New evidence points at `backlog/28`". None of
them were going to find it, because every one asked *where did the reply go* - and no reply was ever
lost.

```rust
paint_view(s);
render::present();
(s.view, max)          // the reply, computed AFTER a full repaint
```

**To answer "move your view", the console had to repaint the framebuffer SYNCHRONOUSLY, inside the
caller's request.** On the Wyse's 3840x2160 panel that is the most expensive thing it does. The
caller allowed one second. `console` is contracted to `core = 0` and the shell was round-robined onto
core 0 as well, so the caller was blocked on the core doing the painting.

Nothing was stuck. **The work did not fit the deadline.**

### Why four rounds missed it

Every instrument asked about DELIVERY, and delivery was never in question:

| round | theory | what actually refuted it |
|---|---|---|
| 1 | a stale cap | the gen-mismatch lines were outside the failure window, and three scrolls succeeded after one |
| 2 | "no long-pass line means the console is healthy" | that report fires at the END of a pass |
| 3 | the SDK reply-mailbox drain | a genuine latent bug, fixed and kept - but it never fired on the failing run |
| 4 | `backlog/28`, a 23 s `net-stack` serve pass | real, in the same window, and not this |

And one instrument was mine and wrong: `our queue N` read the shell TASK's endpoint depth while
`request_with_reply*` waits on the reply MAILBOX, a different endpoint `task_stat` cannot see. It
reported a zero about somewhere the reply was never going to arrive.

**The thing that found it was reading `scroll_view` while building something else.** Recorded because
the lesson is not "measure more": it is that a question asked four different ways is still one
question, and the answer was in the handler the whole time.

### What was deleted

The fix shipped earlier, when `scrollback` became a utility reading BYTES (`utilities/54_scrollback.md`).
This entry closes with the mechanism removed rather than left unreachable:

* `console_scroll` (SDK) - zero callers
* `REQ_SCROLL` and its handler (`services/console`)
* `scroll_view`, `paint_view`, `paint_view_indicator`, `view()`, `take_output_snap`, the
  `view`/`in_view`/`snapped_by_output` state, the `SCROLL_*` action bytes, and `put_num`

Deleted rather than kept: a future caller finding `REQ_SCROLL` in the header would rebuild the exact
shape, and dead code that once caused a bug is an invitation. It also removed state the console kept
ONLY for that feature, including a branch in the OUTPUT path whose sole job was to snap back out of a
view that can no longer exist.

### The general form, which outlives this bug

**The console was both the service being read FROM and the service drawn TO** - one endpoint, one
16-deep queue - so a request made mid-frame could always land behind painting the caller itself had
just asked for. Widening the deadline would have hidden that, not fixed it. What fixed it was moving
the work to the side that was not also the bottleneck.

`backlog/28` remains open and is a different fault.
