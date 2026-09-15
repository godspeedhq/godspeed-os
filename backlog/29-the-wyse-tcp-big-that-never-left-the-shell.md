# 29. A `tcp ... big` on the Wyse that never reached net-stack, and the unbounded wait behind it

**Status: the HANG is fixed and the escape is restored. Why net-stack never answered is NOT
established, and this records both halves so the second is not read as closed by the first.**

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

## Still NOT established: why net-stack never answered

Nothing here explains it. net-stack never logged `tcp ->` and the echo server never saw a `[26]`, so
net-stack never entered the op-21 arm - but net-stack prints nothing when idle, so its silence is not
itself evidence of a wedge. Candidates not separated: the request consumed into the displaced-request
stash and lost, or net-stack blocked inside a nic-driver exchange. Guessing has now been wrong once on
this; the next step is instrumentation.

**The fix above makes that diagnosable for the first time.** With the prompt escapable, the operator
can press `q` and run `events blocked` - which reads in-flight calls live from the kernel - instead of
power-cycling the evidence away.

## What is still open

- Why the Wyse's HID path runs on the 10 ms tick with every hub probe failing (`probes 0/2160 ok`).
  That is its own investigation and is not this branch's work.
- The T630 has still not run this branch at all; the Wyse was booted in its place.
