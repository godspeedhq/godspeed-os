# 29. A `tcp ... big` on the Wyse that never reached net-stack, and the unbounded wait behind it

**Status: CLOSED, hardware-verified on a Dell Wyse 2026-09-16.** Three defects, found in this order
and each hidden by the one in front of it: an unbounded, unescapable wait in the shell; a held request
expired against a guessed deadline instead of its client's; and - the actual cause - a wait that slept
on work already sitting in its own stash. ~20 s per command to ~200 ms.

Kept in full because the WRONG turns are the useful part: two readings were confidently wrong and the
operator's one-keystroke test overturned the first. What moved it each time was an instrument, never
an argument.

## What was reported

On a Dell Wyse (x86-64, RTL8168), `tcp 192.168.4.40 7777 hello` succeeded and
`tcp 192.168.4.40 7777 big` was "stuck". The same two commands run in seconds on a Pi 4 and a
VisionFive 2.

## What the evidence actually says

Three sources, and they agree with each other:

```
23:40:28.496  net-stack: tcp 192.168.4.40:7777 ok - 10 byte(s)        <- `hello`, fine
23:40:28.512  tcp: 10 byte(s) back
23:40:28.512  echo:hello
23:40:28.512  gsh> tcp 192.168.4.40 7777 hellobig                     <- the echo of the NEXT line
23:40:37.768  xhci: alive ...                                         <- and nothing else, for 3 minutes
```

- **No `net-stack: tcp ->` line for the second command.** That line is the first statement in the
  op-21 arm, so net-stack never entered it.
- **The echo server never saw the connection.** `build/echo.log` ends at `[25]` (`hello` from
  192.168.4.49); there is no `[26]`. So no SYN was ever sent.
- **net-stack logged nothing further at all** - which is also exactly what a healthy IDLE net-stack
  looks like. It prints nothing when it has nothing to do, so its silence is not evidence of a wedge.

## The reading I offered was WRONG, and the operator settled it

I read the garbled echo (`tcp 192.168.4.40 7777 hellobig`, and `192.168.4.490` on the line before)
together with `xhci: probes 0/2160 ok` and `a HID report arrived with no interrupt` and proposed that
Enter had never landed - that the shell was at the prompt and nothing was wrong. The discriminating
test was one keystroke, and it came back the other way: **the prompt was dead. No echo, no `q`, and
the machine had to be power-cycled.**

So the shell WAS blocked in `ns_request`, and the console garbling is a separate, pre-existing
artefact (the serial splice) that happened to fit a story.

**What was ruled OUT by reading, not by flashing:** net-stack's three unbounded
`request_with_reply("time", ..)` calls are not a hang. `Call` wakes with `ReplyDead` if the replier
dies (8.6), and `time` itself has no unbounded wait at all - it is a `recv_timeout` loop with
fire-and-forget `try_send`s, so it always answers promptly or is dead. The mutual-block shape 8.9
warns about is not present either: `time` nudges net-stack one-way with `try_send` and awaits nothing.
The send side cannot block indefinitely either - `offer_request` is eight `try_send`s with 2 ms
between them, then it gives up. So the block was in the REPLY wait, which is exactly where a bare
`request_with_reply` parks you.

## The defect, and why the first fix was not enough

`ns_request` - the shell's transaction path for `tcp`, `sock` and `serve`'s listen - waited on
net-stack with **no bound at all**:

```rust
let first = ctx.request_with_reply("net-stack", &msg)...
```

That is Commandment V: nothing above the kernel may halt, and a slow or missing dependency must
RETURN loudly rather than hang. The comment defending it argued that net-stack "sets its own budget
inside and must not be cut short from here" - which is an argument for a GENEROUS bound, not for none.

**Bounding it was necessary and insufficient.** `NET_TXN_SECS = 20` stopped the shell hanging forever,
but a bare `request_with_reply` parks the shell INSIDE the syscall, where it cannot poll the console.
The operator still faced a dead prompt for twenty seconds with no way out. A bound serves the SYSTEM;
`q` serves the PERSON, and only the second one gives the machine back.

So the path is now bounded AND `q`-abortable, via `request_with_reply_qhint` - the same repair
`fs_request_q` already carried for `ls`/`read`/`find`. Conventions rule 10 has said this since it was
written; the networking surface simply never complied. `trace_ask` (the `events` channel) had the
identical shape and is fixed with it, which matters more there: `events blocked` is the instrument you
reach for when something IS wedged.

Two consequences of adding an abort had to be handled in the same change, or it would trade a hang for
a subtler bug:

- **A drain before every request.** An abort leaves a reply that lands afterwards. On the fs channel
  that costs a wrong answer; here it costs a CAPABILITY - a `serve`/`sock` reply carries a listener or
  socket cap that the kernel has already installed (SEC-35), so the next `serve` would
  `take_pending_cap` and get the ABANDONED run's listener, answer on a port it never asked for, and
  release that one on the way out. `drain_stale_net_replies` is the twin of `drain_stale_fs_replies`.
- **Cap reclamation on a discarded tagged reply.** `ns_take_tagged` discarded overtaken replies without
  reclaiming their embedded caps - the same hole, reached by the other door.

**Rule 11 is only partly satisfied, and that is stated rather than glossed.** `q` stops the shell's
wait, not net-stack's transaction, which keeps working its own 8 s budget out. It does not wedge the
next command (the late reply is discarded by its tag), but a `tcp` issued immediately after an abort
can wait behind the one abandoned. Driving the transaction step-by-step from the shell is the full
fix, and it is the same rework `backlog/28` describes for the SNTP dance.

## Verification

- `osdev test shell`: 174 passed, 0 failed, 2 skipped.
- `scripts/tcp_serve_test.py`: PASS (11 checks, both sides) - drives the `serve` listen call site.
- `scripts/tcp_qemu_test.py`: PASS - drives the `tcp` call site, with wire assertions.
- `grep -c 'ctx.request_with_reply('` in the shell is now **0**: no unbounded wait is left anywhere
  above the prompt.

## Confirmed on hardware: the escape works (2026-09-16)

The rebuilt image, run on the Wyse at 192.168.4.30:

```
10:11:21.528  net-stack: tcp 192.168.4.40:7777 ok - 10 byte(s)   <- `hello`, fine
10:11:26.783    (q to quit)                                      <- `big`, the wait lingers
10:11:44.244  tcp: aborted                                       <- q, prompt back
10:13:10.997  tcp 192.168.4.40 7777 big                          <- retried, clean echo this time
10:13:13.787    (q to quit)
10:13:28.857  tcp: aborted                                       <- q, prompt back again
```

Twice, with no reboot. Conventions rule 10 is satisfied on this path now.

**And the console echo is NOT the input.** Attempt 2 echoed as `tcp 192.168,4.40 7777 hello4.40 7777
hello.4.40 7777 hello` - a comma in the address, the tail repeated three times - and net-stack
received a clean `192.168.4.40` with a 5 byte `hello`. So the serial echo on this box is corrupted
independently of what the shell parsed, which is why it supported a wrong reading above. It is a
separate defect (see the still-open list).

## Still NOT established: why net-stack never answered

The bug is now reproducible and clean: `tcp 192.168.4.40 7777 big` twice, echoed correctly the second
time, and **net-stack logged nothing at all** while `hello` seconds earlier worked end to end.

**net-stack is not wedged.** Pinging 192.168.4.30 from the host while the shell was blocked: 4/4
replies, 28-111 ms. ARP and ICMP are answered only from `poll_step`, so the service was looping
normally the whole time. It received the request and lost it.

Eliminated by reading the code, not by flashing:

| candidate | why it is out |
|---|---|
| dropped for no reply cap | reports itself; the latch never fired this boot |
| stash expired (`HOLD_MS`) | reports itself; latch never fired |
| stash full, or body too large | reports itself; latch never fired |
| a stray badge sending it down the capability path | would log AND reply; the shell would not still be waiting |
| op 21 arm entered | its first statement is the `tcp ->` log |
| the send never left | `offer_request` is eight `try_send`s then gives up, and the `(q to quit)` hint only prints from inside the wait, so the send succeeded |

**One silent path remains, and it is the last one uninstrumented.** In `sifted_req`'s closure, a
message with no pending reply cap is taken to BE the driver's answer:

```rust
Some(cap) => { pending.note(ctx, m, badge, cap); false }   // a client: stashed, loud on loss
None      => true,                                          // "the driver's answer"
```

A client request whose reply cap has already been consumed is indistinguishable from a driver reply
there, and taking the second for the first hands it to `nic_req`, which parses it as a frame batch,
discards it, and says nothing. That is the only remaining way to lose a request with no log, and it
fits every observation. **It is NOT asserted as the cause** - reasoning has been wrong twice on this
board already, so it is being measured.

## The instruments added for the next run

1. **An arrival receipt at dispatch**, for ops 21 and 22 only: `net-stack: op 21 reached dispatch
   (from the queue|stash)`. Those two are typed by hand and answered in one round trip, so one line
   each is no flood - unlike the status and accept polls, left deliberately silent. It separates "the
   request never reached dispatch" from "it reached dispatch and the work went wrong", which on this
   board could not be told apart.
2. **The silent path made loud**: the capless arm now reports, once, when it takes a message beginning
   21 or 22 as the driver's answer.

Both are verified to FIRE rather than merely to exist: `op 22 reached dispatch` in
`tcp_serve_test.py`, `op 21 reached dispatch` twice in `tcp_qemu_test.py`, both suites still PASS.

**What the next Wyse run decides.** If the receipt does NOT print for `big`, the request never reached
dispatch and the loss is in the closure - and the second instrument should name it. If the receipt
DOES print, the request arrived and the fault is downstream of it, which is a different and much
smaller search.

## RESOLVED: `big` was never the problem. net-stack is slow to PICK UP a request (2026-09-16)

The arrival receipt settled it in one run:

```
12:36:08.8    shell sends the request       (inferred from the 2 s hint at :10.770)
12:36:10.770    (q to quit)
              ... 18 seconds of silence, from every service ...
12:36:28.798  net-stack: op 21 reached dispatch (from the queue)
12:36:29.077  net-stack: tcp 192.168.4.40:7777 ok - 2884 byte(s)
```

**The transaction takes 279 ms and returns all 2884 bytes correctly, multi-segment (`SADAAF`).** The
protocol is fine. The request sat in the endpoint queue for 20 seconds before net-stack dequeued it.

`hello` has the same disease, milder: Enter at `12:36:00.335`, dispatch at `12:36:05.142` - 4.8 s for a
command that then completes in 142 ms. So it is not about `big`; `big` is the one whose delay crossed
a person's patience. Enter-to-dispatch on one boot:

| when | delay | context |
|---|---|---|
| `12:36:00.335` (`hello`) | 4.8 s | 5 s after the SNTP exchange |
| `12:36:08.8` (`big`) | ~20 s | 8 s after the previous transaction |
| `12:40:38.792` (`big`) | 0.49 s | after 4 minutes idle |

**It is not a warm-up.** A later command was the slowest and a much later one the fastest. What
correlates is RECENT NETWORK ACTIVITY.

Two earlier "hangs" are explained too: `q` was pressed 19.4 s and 17.1 s after the send, both just
short of the delay. They would have returned.

## And it is `backlog/28`, reproduced in QEMU

The slow-pass detector added here fires **9 times in one `osdev test shell` run**, so this is not
Wyse-specific and needs no board to chase. Every one of them follows a long in-loop operation:

```
gsh> ping count 3 10.0.2.2        <- a ping sequence, ~900 ms per window
net-stack: DHCP - ACK ...          <- the dance
net-stack: SNTP - querying ...     <- the SNTP exchange
```

net-stack is single-threaded and runs the dance, SNTP and a ping sequence INSIDE its serve loop. While
one runs it does not ask for client requests, so a request arriving during it waits for the whole
thing. That is exactly the limitation `backlog/28` records and `docs/tcp-design.md` already states
about this service - **the fix is making the dance incremental, a state-machine rework rather than a
constant.** This entry adds the measurement that makes the cost visible per occurrence; it does not
change the fix.

**Confirmed by chaos on hardware.** After `chaos 100 rounds` the operator reported `big` slow again -
as predicted here before the run: chaos restarts net-stack repeatedly, every restart re-runs the
DHCP + ARP + SNTP dance, and the dance is precisely what blocks the loop.

## The instrument that settled it, and the one added after

1. **Arrival receipt at dispatch**, ops 21 and 22 only: `net-stack: op 21 reached dispatch (from the
   queue|stash)`. Separated "never reached dispatch" from "reached dispatch and the work went wrong".
   Answer: it reaches dispatch, very late, from the QUEUE (so it was not stashed, and the shell never
   retried - there is no `discarded` line, so the whole wait was one tag).
2. **Slow-pass report**: a serve pass over `SLOW_PASS_MS` (1 s) says how long it took. A healthy pass
   is `POLL_MS` + `POLL_BUDGET_MS` = 350 ms, so the threshold is four times the worst legitimate pass
   and a compile-time assertion keeps it that way. This is what makes the starvation self-reporting on
   any board instead of a mystery per platform.

## THE ACTUAL BUG: the wait slept on work it already had (2026-09-16)

Making the drop report a sentence found it in one run:

```
13:22:02.842  net-stack: dropped a held client request (op 21) after its client's own 20000 ms of patience
13:22:02.864  net-stack: a serve pass took 2060 ms (over 1000)
13:22:28.977  net-stack: op 21 reached dispatch (from the stash)
```

The request was held for its client's FULL twenty seconds and never served, while the loop was running
normally - only 2 s of slow pass in the whole window. And every successful "from the stash" dispatch
landed immediately after an unrelated dispatch had woken the loop.

**The stash is drained only by `pending.take()` at the top of the serve loop, and the wait below it
only exits when a NEW message arrives.** So a request displaced into the stash by the poll step sat
there until something unrelated happened along. For a shell blocked on that very request, nothing ever
did: it aged out its entire patience, was dropped, the client re-sent, and the re-send was answered in
milliseconds. That is why every served copy arrived "from the queue" - it was the retry.

The fix is three words at the bottom of the wait: `if pending.has_work() { continue 'serve; }`. Going
back to the top of the serve loop reaches `take`, which is the only thing that drains the stash.

**This was the real defect all along.** The two earlier fixes were both necessary and neither was it:
the 20 s bound stopped an unbounded hang, and the per-client patience byte stopped net-stack
discarding a 20 s client's request after 1.5 s - but a request that is never taken is dropped whatever
its deadline says. The instruments are what made each layer visible: the arrival receipt showed the
served copy was the retry, the slow-pass report killed the starvation theory, and the unlatched drop
line named the victim and its patience.

Measured effect in `osdev test shell`: patience-expiry drops **2 -> 0**. The one remaining drop is a
full stash during a burst, which is a different and bounded case.

## Also still open

- **The console echo corrupts characters on this box** - duplicated runs, inserted commas - while the
  shell's input buffer is fine. Alongside `xhci: probes 0/2172 ok` (every hub probe failing) and `a
  HID report arrived with no interrupt - polling input at the 10ms tick`. Its own investigation.
- The T630 has still not run this branch at all; the Wyse was booted in its place.

## Closed on hardware (2026-09-16)

```
13:59:48.285  op 21 reached dispatch (from the queue)   ->  189 ms   big
13:59:49.820  op 21 reached dispatch (from the stash)   ->  279 ms   hello
13:59:51.288  op 21 reached dispatch (from the queue)   ->  252 ms   big
```

The middle line is the evidence that matters: **a request served FROM THE STASH in 279 ms**, the exact
path that previously aged out its client's full patience and was dropped. Across the run: no
`(q to quit)`, no drops, no timeouts.

### What each fix was worth, in order

| | |
|---|---|
| `NET_TXN_SECS` + `q` | a hang became a bounded, escapable failure. Necessary; not the cause |
| the patience byte | net-stack stopped discarding a 20 s client's request after 1.5 s. Necessary; not the cause |
| `if pending.has_work() { continue 'serve; }` | the cause |

### What actually did the work

Not reasoning - reasoning was wrong twice. Each step forward came from an instrument, and each
instrument was cheap:

- **the arrival receipt** showed the served copy was the RETRY ("from the queue"), not the original;
- **the slow-pass report** killed the starvation theory by measuring 2 s of block inside a 20 s wait;
- **un-latching the drop line** named the victim and its patience in one run.

The two "said once" latches deserve their own note: both were added in good faith to stop a flood, and
between them they hid this bug for three sessions. A latch reports the first occurrence and then
asserts silence, which is indistinguishable from health. Rate-limiting reports the twentieth. §26.7
asks for a failure to stay visible, and "visible once, at boot" is not that.
