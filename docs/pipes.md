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
the sink consumes it. Each inter-stage buffer is **64 KiB** (loud on overflow, §26.6); it lives
on the user stack — two coexist for a middle filter (input + output ≈ 128 KiB), within the
256 KiB user stack.

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

Exchanging **whole buffers — one message each way** — keeps the bounded IPC queues deadlock-free
(§8.9): neither side ever has more than a couple of messages queued. The shell's single endpoint
is reused **sequentially** down the chain (one service alive at a time). A message is 4 KiB, so a
stage crossing a **service** boundary is capped at 4 KiB and refuses a larger buffer loudly
(it is not chunked — see *Streaming* below).

## The EOT end-of-stream marker

A service ends a stream with a one-byte **EOT** (`0x04`) message, so the shell's `recv` drain
knows when to stop. A zero-length message is **not** used — the IPC path does not deliver an
empty body. A filter (`upper`) forwards EOT downstream; the shell stops draining on it.

## Bounds and failure (loud, never silent — §26.6 / §3.12)

- Each inter-stage buffer is 64 KiB; overflow is reported, not silently truncated.
- A pipeline is capped at `MAX_STAGES` (8); more is refused.
- **Stage 1 must be a producer.** A non-producer service in stage 1 would block the shell on a
  `recv` that never comes (no non-blocking `recv` in v1), so producer services are an explicit
  whitelist (`is_pipe_producer_service`, currently `greet`); anything else is refused loudly.
- A producer built-in mid-pipe or as a sink (it ignores input), or a service that never
  registers when used as a filter, is reported, not silently mishandled.
- A buffer larger than a **service** can take (4 KiB) or a **file** can hold (~3.5 KiB) is
  refused loudly with the actual size and the reason — it is never silently clipped.

## Why store-and-forward, and the chain of real limits

This is **store-and-forward**, not a true stream: each stage runs to completion, its whole
output is materialised into the 64 KiB buffer, and *then* the next stage runs. Stages run
**sequentially**, one at a time — never concurrently. That is a deliberate v1 choice: it
sidesteps the hardest part of real pipes — §8.9, where the kernel will *not* detect or break a
deadlock, and a concurrent producer/consumer needs backpressure. Store-and-forward has exactly
one service alive at a time and one bounded message each way, so it is provably deadlock-free.

The cost is that it is bounded in *total* data, not just in buffer size. After the 64 KiB bump
the buffer is no longer the binding limit; **three smaller ceilings are**, and they are all the
same "no streaming / no multi-block" limitation:

| Limit | Value | Set by |
|-------|-------|--------|
| IPC message | 4 KiB | `MAX_PAYLOAD` (§8.5) — a stage through a *service* is one message |
| File write | ~3.5 KiB | `fs` `MAX_FILE_BYTES` — `\| write` is one `WriteFile` (no multi-block files) |
| Concurrency | none | stages run sequentially; data is materialised, not flowing |

So a *builtin-only* capture can fill 64 KiB, but it can only reach a sink that can take it —
which today nothing beyond 4 KiB can. Lifting this is the streaming work, not a constant.

## Future: true streaming (design intent — not built)

A streaming pipeline would match the POSIX mental model — `a | b | c` running **concurrently**,
bytes **flowing through** bounded queues, total data unbounded. It is a deliberate future
project, not a tweak, and it is where §8.9's deadlock discipline stops being theoretical. The
pieces:

1. **Concurrent stages.** Each stage is a live task for the pipeline's duration (not spawned and
   reaped one at a time). The shell wires stage *i*'s output cap to stage *i+1*'s endpoint, so
   data flows producer→…→sink without the shell materialising any stage in full.
2. **Backpressure.** With bounded per-endpoint queues (depth 16, §8.5), a fast producer must
   block on a full queue until the consumer drains it — `send` already blocks, so the queue *is*
   the backpressure, but every stage must then use the **`try_send`/structured discipline** §8.9
   requires so a stall can't become a deadlock.
3. **Multi-block files.** `\| write` of a large stream needs `fs` to write a file across many
   blocks (chunked `WriteFile`), lifting the ~3.5 KiB ceiling — its own `fs`/block-IPC change.
4. **A streaming filter contract.** A filter service reads a chunk, emits a chunk, repeats —
   `upper` is already shaped this way (chunk-in → chunk-out); the change is the shell *not*
   draining it in full but forwarding each chunk onward.

The store-and-forward machinery here is the foundation: the producer/filter/sink roles, the EOT
marker, and the whole-buffer round-trip generalise to chunked streaming without changing the
shape — only *who drains whom, and when*.

## Filter built-ins

A **filter built-in** consumes the previous stage's buffer and emits to the next — it runs
in-process, so it is **not** subject to the 4 KiB service-boundary cap and can filter a full
64 KiB buffer. Built so far: **`match`** (grep — `utilities/27_match.md`), **`count`** (wc —
`utilities/28_count.md`), and **`sort`** (`utilities/29_sort.md`): `read /log | match error |
sort | count`. More — `head`/`tail` — and piping into command arguments are the remaining
Appendix-D work; each drops into the same middle FILTER slot.
