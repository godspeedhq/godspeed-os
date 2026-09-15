# 28. A listener's port is released by the CLIENT, and sometimes is not

**Status: OPEN.** Intermittent, measured, diagnosable, and not fixed. The workaround is a bounded
retry that did not measurably help; the real fix is a design change, described below.

## What happens

`serve <port>` asks `net-stack` to listen, and asks it to stop on the way out. When that second ask
does not land, the port stays registered and the next `serve` on it is refused:

```
net-stack: cannot listen on TCP port 8080 - that port is already taken, or every listener slot is in use
serve: net-stack would not listen on that port - see its log for why
```

`MAX_LISTEN` is 2, so two leaked ports and the machine cannot listen at all until `net-stack`
restarts.

## How often

Measured with `scripts/tcp_serve_test.py`, which runs `serve` twice on one port and connects to each:

| | runs | failures |
|---|---|---|
| before the retry | 10 | 3 |
| after the retry (3 attempts, 150 ms apart) | 12 | 2 |

**The retry did not measurably help**, and the sample is too small to prove it did anything at all.
It is kept because it cannot hurt and because §26.7 asks for a failed recovery to be retried, not
because it is evidence of a fix.

## What is established

- The failure is real and reproducible in aggregate, not a test artefact. A failing run shows the
  second connection answered by the FIRST run's listener, which is still registered.
- It is visible now. `serve` prints `the port was NOT released` when its release call fails, and
  `net-stack` logs a badged invocation that matched no resource it owns. Before those, a release
  that never happened was indistinguishable from one that did.
- On at least one failing run NEITHER line printed, so the release was not merely refused - it was
  not reached. That path is not yet understood.

## One real cause found and fixed (2026-09-15)

**Two budgets collided.** `HOLD_MS` (how long net-stack keeps a displaced client request) and
`POLL_BUDGET_MS` (how long a poll step may run) were both 500 ms, set an hour apart and never
compared. A request stashed at the START of a poll expired at exactly the moment that poll finished.

The board named it, once the drop was made loud:

```
received 10 byte(s): second run
net-stack: a held client request waited more than 500 ms and was dropped
serve: the echo was not accepted
```

The echo request was eaten by the very stash meant to protect it. Fixed: `HOLD_MS` 1500,
`POLL_BUDGET_MS` 250, and the ordering `POLL_BUDGET_MS < HOLD_MS < shortest client deadline` is now a
compile-time assertion rather than a comment - a comment is what failed.

That was almost certainly the cause of the two hardware failures. A residual flake remains in QEMU,
about one run in seven, and is NOT explained by it.

## The residual flake is SNTP blocking the serve loop (2026-09-15, Pi 2)

Not a listener problem at all, and not new. The board:

```
19:29:59.811  net-stack: SNTP - querying 139.162.219.252:123
19:30:05.501  net-stack: `time` asked for the clock - no SNTP answer
19:30:05.503  net-stack: a held client request waited more than 1500 ms and was dropped
19:30:07.783  accepted a connection (1)
19:30:12.514  the peer connected but sent nothing
```

**net-stack was blocked for 5.7 seconds inside one SNTP query.** For that whole window it served
nobody: the held request expired, the next connection's handshake took 3.3 s, and its data segment
was lost. Two of three connections failed; the two that succeeded did not overlap an SNTP attempt.

This is the limitation the service already documents about itself, quoted in `docs/tcp-design.md`:

> the in-loop dance still blocks this service while it runs ... Making the dance incremental so
> net-stack answers THROUGHOUT it is the real fix, and that is a rework of the state machine rather
> than a constant.

`serve` is simply the first long-running command that overlaps `time`'s periodic clock nudges, so it
is the first thing to make the cost visible. The same correlation is present in the QEMU flake.

**So the fix is the incremental dance, not anything in this file's title.** Until then a `serve`
session will lose a connection whenever an SNTP query stalls, and the board says so on both lines.
The parts of this entry above - the release being the client's job, and the two budgets that
collided - are real and separate; this is what remains after both.

## It reaches the x86 shell suite too (2026-09-15)

`ping count 3` and `net stats` fail intermittently in `osdev test shell` - 174/0 twice and 172/2
twice on the same build, so it is load-dependent rather than deterministic. The new diagnostics make
the whole chain legible in five lines:

```
shell: discarded a net-stack reply for tag 4 while awaiting 5 (an earlier request was overtaken)
net-stack: a client request met mid-question to nic-driver was dropped because the stash was full
No reply from 10.0.2.2: net-stack not responding
No reply from 10.0.2.2: net-stack not responding
net-stack: SNTP - querying 185.51.192.62:123
```

`time` nudges net-stack for the clock, the SNTP dance blocks it for seconds, the shell's ping
requests pile into the stash, `STASH_N` (4) fills, and the rest are dropped. Same root cause as the
section above.

**Raising `STASH_N` would hide this rather than fix it.** The queue behind it is 16 deep and a
multi-second block will fill any bound worth having; the fix is for the dance not to block.

Worth noting what the tag bought here: every one of those overtaken replies would previously have
been READ as the answer to the wrong request. A visible timeout and a retry is the better failure,
and it is the one the test now reports.

**Not attributed to a commit.** The suite passed on this branch earlier the same day, so something
made it more likely, but that was not established by bisection and is not claimed.

## What is NOT established

Why the call fails. Candidates not yet separated: a race with `net-stack` reaping and revoking the
CONNECTION alongside it, the shell's pending-capability FIFO being disturbed by the reap's revoke, or
the invocation timing out against a `net-stack` that is mid-poll. Guessing has been wrong three times
on this already; the next step is instrumentation on the failing path, not another patch.

## The real fix, and why it is a design change

**A port cannot release itself, because the kernel does not tell a service when a capability is
dropped.** So `net-stack` has to trust a client to say "I am done", and a client that dies, is
killed, or simply races will always leak the port. The retry narrows the window; it cannot close it.

That asymmetry is the actual defect. `fs` has the same shape for files and lives with it; a listener
is worse, because there are only two of them and they are named by a number a user picks.

Two honest routes:

1. **`net-stack` owns the lifetime.** A listener expires unless the holder renews it, or is released
   when its resource generation is bumped by something the service can observe. Needs a mechanism
   the kernel does not currently offer.
2. **A way to reclaim by name.** `serve 8080 stop`, or a `net listeners` verb that shows what is
   registered and can drop one. Ugly, but it makes the leak recoverable without a restart, which is
   the part that actually hurts.

Neither is a constant, so per §26.7 this is recorded rather than half-built.

## Not to be confused with

The listener leak fixed in `9f160761`, which was unconditional: dropping the capability told
`net-stack` nothing and `unlisten` was wired to nothing at all. That is fixed and verified on a
Raspberry Pi 2. This item is the residue - the release now exists and usually works.
