# 29. A `tcp ... big` on the Wyse that never reached net-stack, and the unbounded wait behind it

**Status: the HANG is fixed. The CAUSE is not established, and this records both halves so the
second is not read as closed by the first.**

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

## The reading the evidence supports, stated as a reading

**The command line was probably never submitted.** The echoed line reads
`tcp 192.168.4.40 7777 hellobig` - the word `hello` from the PREVIOUS command's output spliced into
the line being typed - and the first command echoed as `192.168.4.490` while net-stack correctly
received `.40`. On the same boot, `xhci` reports `probes 0/2160 ok` and
`a HID report arrived with no interrupt - polling input at the 10ms tick`. The input and echo path on
this box was running degraded for the whole session.

If Enter never landed, every observation above follows with nothing else wrong: the shell sat at
`console_read`, net-stack was idle and correct, and no SYN was sent because none was asked for.

**What was ruled OUT by reading, not by flashing:** net-stack's three unbounded
`request_with_reply("time", ..)` calls are not a hang. `Call` wakes with `ReplyDead` if the replier
dies (§8.6), and `time` itself has no unbounded wait at all - it is a `recv_timeout` loop with
fire-and-forget `try_send`s, so it always answers promptly or is dead. The mutual-block shape §8.9
warns about is not present either: `time` nudges net-stack one-way with `try_send` and awaits nothing.

## What discriminates it

One keystroke, no reflash: **type at the prompt.** A prompt that still echoes and still runs a command
was never hung, and this entry is about a keyboard. A prompt that is dead means the shell WAS blocked,
and the fix below is the one that matters.

## The defect that WAS real, and is fixed

Whichever of the two it was, `ns_request` - the shell's transaction path for `tcp`, `sock` and
`serve`'s listen - waited on net-stack with **no bound at all**:

```rust
let first = ctx.request_with_reply("net-stack", &msg)...
```

That is Commandment V: nothing above the kernel may halt, and a slow or missing dependency must
RETURN loudly rather than hang. The comment defending it argued that net-stack "sets its own budget
inside and must not be cut short from here" - which is an argument for a GENEROUS bound, not for
none. It is now `NET_TXN_SECS = 20`, derived from what the far side can legitimately take: 8 s for
`tcp_transact`'s own budget, plus the several seconds a request can queue behind a blocking SNTP
dance (`backlog/28`, measured at 5.7 s).

`ns_request` is now one line calling `ns_deadline`, because the missing bound was the only thing that
had ever distinguished the two functions.

**This does not depend on the cause above being right.** An unbounded wait above the kernel is a
violation whether or not it fired here, and if it did fire, a shell that says
`tcp: net-stack did not answer` after 20 s is the correct failure where a dead prompt was not.

## What is still open

- Why the Wyse's HID path runs on the 10 ms tick with every hub probe failing (`probes 0/2160 ok`).
  That is its own investigation and is not this branch's work.
- The T630 has still not run this branch at all; the Wyse was booted in its place.
