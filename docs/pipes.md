# Pipes — composing built-ins and services

> **Status:** Implemented (`osdev test files`); two-stage pipes hardware-verified on the T630,
> multi-stage QEMU-verified. Realises **Appendix D.3** (capability-mediated pipes). Trails
> `CLAUDE.md`; does not amend it.

`A | B | C | …` feeds each stage's output into the next. Unlike a POSIX pipe (an inherited file
descriptor across a `fork`), a GodspeedOS pipe is **capability-mediated**: the shell brokers a
real endpoint cap (or an in-process capture) between stages. There is no shared buffer and no
ambient authority — data always crosses a real boundary (§26.4 forbids a silent shell-internal
data path masquerading as a pipe).

## The stage model

A pipeline is **one producer, zero or more filters, one sink**:

```
 stage 1        middle stages         last stage
 PRODUCER  |    FILTER | FILTER  |     SINK
```

- **PRODUCER** — emits text, ignores input. Built-ins: `read`/`cat`, `echo`, `ls`, `tree`,
  `find`. Service: `greet`.
- **FILTER** — consumes input, emits output. Service: `upper` (re-emits its result, so it can
  sit *anywhere* in a chain). A future filter built-in (`match`) slots in here.
- **SINK** — consumes the final buffer. Built-in: `write <file>`. A service filter used as the
  last stage prints its output to the console; with no recognised sink, the buffer is printed.

The shell threads a bounded buffer down the chain: stage 1 fills it, each filter transforms it,
the sink consumes it. The buffer is one message (4 KiB; loud on overflow, §26.6).

Examples:

```
echo hello | upper                  builtin producer → service filter → console
tree / | write /snap.txt            builtin producer → write sink
read /log | upper | write /up.txt   producer → filter → sink (3 stages)
greet | upper | write /g.txt        service producer → service filter → sink
```

## How a built-in stage works

A producer built-in renders through an `Out` target that is either the console or a capture
buffer; in a pipe it captures. The `write` sink writes the buffer to a file. No `fs` surface is
added — built-ins already had these capabilities; the pipe just redirects their text.

## How a service stage works (the round-trip)

A service stage is wired with **no new syscall**:

1. The shell spawns the service with `spawn_pipe(service, "shell")`. The delegated SEND cap to
   the shell's own endpoint is installed **first** (`send_peer_at(0)`) — that is the service's
   "downstream". The service's contracted peers (e.g. `registry`) follow, so a filter that must
   register to receive input still can.
2. If the stage has input (a filter/sink, not stage 1), the shell resolves the service's
   endpoint with `registry_lookup` (the service self-registers) and sends the input buffer as
   **one message**, then an **EOT**.
3. The service processes and sends its output back to the shell (`send_peer_at(0)`), ending with
   an **EOT**.
4. The shell drains its endpoint until EOT, then reaps the service.

Exchanging **whole buffers — one ≤4 KiB message each way** — keeps the bounded IPC queues
deadlock-free (§8.9): neither side ever has more than a couple of messages queued. The shell's
single endpoint is reused **sequentially** down the chain (one service alive at a time).

## The EOT end-of-stream marker

A service ends a stream with a one-byte **EOT** (`0x04`) message, so the shell's `recv` drain
knows when to stop. A zero-length message is **not** used — the IPC path does not deliver an
empty body. A filter (`upper`) forwards EOT downstream; the shell stops draining on it.

## Bounds and failure (loud, never silent — §26.6 / §3.12)

- Each inter-stage buffer is one 4 KiB message; overflow is reported, not silently truncated.
- A pipeline is capped at `MAX_STAGES` (8); more is refused.
- **Stage 1 must be a producer.** A non-producer service in stage 1 would block the shell on a
  `recv` that never comes (no non-blocking `recv` in v1), so producer services are an explicit
  whitelist (`is_pipe_producer_service`, currently `greet`); anything else is refused loudly.
- A producer built-in mid-pipe or as a sink (it ignores input), or a service that never
  registers when used as a filter, is reported, not silently mishandled.

## Not yet (Appendix D)

A general filter built-in set — **`match`** (grep-equivalent, see `utilities/27_match.md`),
`count`, `sort`, `head`/`tail` — and piping into command arguments. The multi-stage machinery
here is the foundation: `match` drops straight into a middle FILTER slot.
