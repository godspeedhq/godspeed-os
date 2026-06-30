# Pipes - composing built-ins and services

> **Status:** Implemented (`osdev test files`); two-stage pipes hardware-verified on the T630,
> multi-stage QEMU-verified. Realises **Appendix D.3** (capability-mediated pipes). Trails
> `CLAUDE.md`; does not amend it.

`A | B | C | …` feeds each stage's output into the next. Unlike a POSIX pipe (an inherited file
descriptor across a `fork`), a GodspeedOS pipe is **capability-mediated**: the shell brokers a
real endpoint cap (or an in-process capture) between stages. There is no shared buffer and no
ambient authority - data always crosses a real boundary (§26.4 forbids a silent shell-internal
data path masquerading as a pipe).

> **A pipe now carries text *or* records.** The dispatcher threads a `Stream` that is either a
> byte buffer or a typed `Table`; `from json` / `to json` flip between them, and each stage is
> chosen by its command *and* by which it currently holds. This page describes the **text**
> world (producers, filters, sinks, services); the **record** world (`where`/`select`/`sort`,
> `from`/`to`) is in `docs/records.md`. They are one pipeline.

## The stage model

A pipeline is **one producer, zero or more filters, one sink**:

```
 stage 1        middle stages         last stage
 PRODUCER  |    FILTER | FILTER  |     SINK
```

- **PRODUCER** - emits text (or records), ignores input. Built-ins: `read`, `echo`, `ls`, `tree`,
  `find`, the system-info commands `about` / `mem` / `cores` / `date` / `help`, and the introspection
  producers `status` / `caps` / `drives` / `observe now`. Services: `greet` (text),
  `roster` (records). *(There is no `cat`: `read` is the one file reader - `utilities/18_read.md`,
  the replacement for POSIX `cat`, whose name describes a different operation. This OS does not
  carry POSIX vocabulary for its own sake.)*
- **FILTER** - consumes input, emits output. Service: `upper`. Built-ins: `match`/`count`/`sort`/
  `first`/`last` (text) and the record verbs `where`/`select`/`sort`/`from`/`to` (`docs/records.md`).
- **SINK** - consumes the final buffer. Built-in: `write [append|prepend] <file>` (plain
  overwrites; the keywords add to the end / front - see *The `write` sink* below) and `assert`
  (the verifying sink). A service filter used as the last stage prints to the console; with no
  recognised sink, the buffer is printed.

The shell threads a bounded buffer down the chain: stage 1 fills it, each filter transforms it,
the sink consumes it. Each inter-stage buffer is **64 KiB** (loud on overflow, §26.6); it lives
on the user stack - two coexist for a middle filter (input + output ≈ 128 KiB), within the
256 KiB user stack.

Examples:

```
echo hello | upper                  builtin producer → service filter → console
tree / | write /snap.txt            builtin producer → write sink
read /log | upper | write /up.txt   producer → filter → sink (3 stages)
greet | upper | write /g.txt        service producer → service filter → sink
about | write /about.txt            capture any text producer's output to a file
```

## What can start a pipe - the producer rule

The governing idea is simple: **anything that displays information can be saved.** A command is a
pipe source iff its job is to *emit data*. That splits the command set three ways:

- **Data / display commands → pipe sources.** Anything whose purpose is to show you something:
  `about`, `mem`, `cores`, `date`, `help`, `status`, `ls`, `caps`, `drives`, `find`, `tree`,
  `read`, `echo`, `observe now`. Each renders through an `Out` target that is the console when run
  bare and a capture buffer when piped - so `about` prints, and `about | write /f` saves, the same
  bytes. No new authority: a built-in already held these capabilities; the pipe just redirects its
  text.
- **Action / mutation commands → NOT sources.** `spawn`, `kill`, `restart`, `reboot`, `mkdir`,
  `copy`, `move`, `rename`, `delete`, `cd`, `write`, `edit`, `drives flash`. These *do* something;
  their one-line "ok"/error is an **acknowledgement, not data**. And that outcome already has its
  own channel - the Result model (`result`, `assert ok <cmd>`). Piping `delete /x | write log`
  to capture the word "ok" would blur the data channel and the outcome channel, which the shell
  keeps deliberately apart. A non-producer in stage 1 is refused loudly ("… cannot start a pipe").
- **Live / interactive → NOT sources.** The full-screen `observe` live view and `edit` own the
  screen and never yield a discrete stream; piping them is a loud refusal (use `observe now`).
- **Orchestrators (`run` / `selfcheck`) → NOT sources.** They run the suite's *own* sub-pipelines,
  so capturing one through a pipe would nest a `pipe_run` (which holds a 64 KiB `Stream` on the
  stack) inside another - two coexisting 64 KiB buffers overflow the tight user stack (HW-proven).
  They refuse loudly as non-producers. **To save an orchestrator's output, it writes its OWN file:**
  `selfcheck save <path>` / `run <script> save <path>` streams the report straight to a file
  (direct, no pipe - a small bounded buffer, not a nesting capture), then `read <path> | …` brings
  it into the pipe world (`read` *is* a leaf producer). That is the right shape anyway: a report is
  a **durable artifact** (keep it, re-read it, grep it, `edit` it), and a file decouples producing
  it from consuming it - a pipe couples them in time and is for transient streams. (To build a big
  file for `edit`'s windowing instead, append a *simple* producer a few times: `help | write
  /big.txt` then `help | write append /big.txt` ×N.)

## Materialize, then pipe - the standing escape hatch

> **When something can't be a direct pipe source - an orchestrator, output too large for the
> capture buffer, anything that won't fit the transient model - don't reach for an in-memory
> mechanism. Stage it through a file: materialize it (the utility writes its own file), then pipe
> from the file.**

This is the move to reach for *before* building machinery (a streaming sink, a buffer pool, a
heap). It needs nothing new: a utility that can write a file, and `read` (a leaf producer) that
pipes one. The **file is the adapter** between two things that couldn't meet directly - the cheapest,
most universal one in the system: a named, bounded, durable artifact.

```
selfcheck save /report.txt        # step 1: produce → file (direct, no pipe)
read /report.txt | match FAIL     # step 2: file → pipe (read is a leaf producer)
```

It is usually the *better* shape, not a compromise. A pipe couples producer and consumer in time
(shared transient buffer, gone once consumed). A file turns that into a **checkpoint**: produce
once, consume later, repeatedly, however you like - re-runnable, inspectable, resumable. This is the
Unix "everything is a file" instinct (and Plan 9's), and it's how systems scale *past* memory: `sort`
spills to a tempfile, MapReduce writes map output to disk before the shuffle, build systems
checkpoint intermediate artifacts. "Materialize between stages" is the pattern that survives when the
data won't fit in RAM and when you want fault tolerance (a disk checkpoint is restartable).

Honest caveat: staging costs a disk round-trip and the artifact's space, and it is *not* concurrent
(step 1 finishes before step 2 starts). For a **bounded, durable result** that is the right trade
every time. Only a truly **unbounded flow through filters** justifies the real concurrent streaming
pipeline - until then, materialize, then pipe. (Same shelf as "resist the heap", `CLAUDE.md`
§26.6.1: reach for a bounded, durable, visible mechanism before an in-memory clever one.)

## The `write` sink - overwrite by default, append/prepend explicit

`write` is the file sink, identical in the pipe (`… | write <path>`) and standalone
(`write <path> <text>`) forms:

| Form | Effect |
|------|--------|
| `write <path>` | **Overwrite** (or create). The default - the common case is unqualified. |
| `write append <path>` | Add to the **end** (create if missing). |
| `write prepend <path>` | Add to the **front** (create if missing). |

The destructive-vs-additive choice is a **visible keyword**, not a punctuation subtlety - you can
read the line and know exactly what it does. Append/prepend stream through a temp file
(`fs_stream_combine`): the original is read while the combined content is written to a staging file
that then atomically replaces the target, so they work on files of any size with constant memory.
`prepend` is honestly a **full-file rewrite** (there is no insert-at-front in the filesystem), so
it costs the same as rewriting the file - stated, not hidden (§26.7).

## Why there is no `>` redirection

A POSIX reflex says "where's `>`?". The answer is that `>` would be **redundant here, not
ergonomic**. In POSIX, `>` exists because redirection (an `fd` dup) is a *different primitive* from
a pipe (`fork` + `pipe`). In GodspeedOS both are the **same** mechanism - a capability to a sink -
so `| write` *is* "redirection as capability minting" (Appendix D.2). Adding `>` would be a second
syntax for one mechanism, exactly the kind of speculative convenience surface §26.2 / §26.5 tell us
to resist - and it drags its baggage in (`>>` vs `>`, clobber/noclobber, `2>`), plus parser
special-casing for a literal `>` in `echo` text. The verb form is self-documenting and keeps the
authority (the `fs` write-cap) visible at the word `write`, where it is exercised (Appendix D.4).

## Type mismatches are loud (text vs records)

A pipe carries text **or** records, and a stage that gets the wrong kind fails loudly and aborts
the pipeline (§3.12) - it never silently passes garbage through:

```
about | to json   → "to: input is text, not records (parse with 'from json' first)"
ls | to xml        → "to: unknown format (try: to json | to yaml)"
status | match x   → "match: this is a record stream - use 'where'/'select'/'sort', or 'to json'"
```

The new text producers (`about`/`mem`/…) emit a **byte** stream, so feeding one into a record verb
(`to json`, `where`, …) trips this guard automatically - no special-casing needed.

## How a built-in stage works

A producer built-in renders through an `Out` target that is either the console or a capture
buffer; in a pipe it captures. The `write` sink writes the buffer to a file. No `fs` surface is
added - built-ins already had these capabilities; the pipe just redirects their text.

## How a service stage works (the round-trip)

A service stage is wired with **no new syscall**:

1. The shell spawns the service with `spawn_pipe(service, "shell")`. The delegated SEND cap to
   the shell's own endpoint is installed **first** (`send_peer_at(0)`) - that is the service's
   "downstream". The service's contracted peers (e.g. `fs`) follow, so a filter that must
   reach them to receive input still can.
2. If the stage has input (a filter/sink, not stage 1), the shell resolves the service's
   endpoint by name via the kernel directory (the kernel records the name at spawn) and sends the input buffer as
   **one message**, then an **EOT**.
3. The service processes and sends its output back to the shell (`send_peer_at(0)`), ending with
   an **EOT**.
4. The shell drains its endpoint until EOT, then reaps the service.

Exchanging **whole buffers - one message each way** - keeps the bounded IPC queues deadlock-free
(§8.9): neither side ever has more than a couple of messages queued. The shell's single endpoint
is reused **sequentially** down the chain (one service alive at a time). A message is 4 KiB, so a
stage crossing a **service** boundary is capped at 4 KiB and refuses a larger buffer loudly
(it is not chunked - see *Streaming* below).

## The EOT end-of-stream marker

A service ends a stream with a one-byte **EOT** (`0x04`) message, so the shell's `recv` drain
knows when to stop. A zero-length message is **not** used - the IPC path does not deliver an
empty body. A filter (`upper`) forwards EOT downstream; the shell stops draining on it.

## Bounds and failure (loud, never silent - §26.6 / §3.12)

- Each inter-stage buffer is 64 KiB; overflow is reported, not silently truncated.
- A pipeline is capped at `MAX_STAGES` (8); more is refused.
- **Stage 1 must be a producer.** A non-producer service in stage 1 would block the shell on a
  `recv` that never comes (no non-blocking `recv` in v1), so producer services are an explicit
  whitelist (`is_pipe_producer_service`, currently `greet`); anything else is refused loudly.
- A producer built-in mid-pipe or as a sink (it ignores input), or a service that never
  registers when used as a filter, is reported, not silently mishandled.
- A buffer larger than a **service** stage can take (4 KiB, one message) is refused loudly with
  the actual size and the reason - never silently clipped. (The **`write` sink** is *not* limited
  this way: it streams the captured buffer to a multi-block file via `WriteNew`/`WriteAt`, so it
  can save up to the full 64 KiB capture.)

## Why store-and-forward, and the chain of real limits

This is **store-and-forward**, not a true stream: each stage runs to completion, its whole
output is materialised into the 64 KiB buffer, and *then* the next stage runs. Stages run
**sequentially**, one at a time - never concurrently. That is a deliberate v1 choice: it
sidesteps the hardest part of real pipes - §8.9, where the kernel will *not* detect or break a
deadlock, and a concurrent producer/consumer needs backpressure. Store-and-forward has exactly
one service alive at a time and one bounded message each way, so it is provably deadlock-free.

The cost is that it is bounded in *total* data, not just in buffer size. After the 64 KiB bump
the buffer is no longer the binding limit; **three smaller ceilings are**, and they are all the
same "no streaming / no multi-block" limitation:

| Limit | Value | Set by |
|-------|-------|--------|
| IPC message | 4 KiB | `MAX_PAYLOAD` (§8.5) - a stage through a *service* is one message |
| Capture buffer | 64 KiB | the inter-stage buffer; a builtin-only pipeline is bounded by this |
| Concurrency | none | stages run sequentially; data is materialised, not flowing |

(The `\| write` sink is no longer a ~3.5 KiB ceiling: it streams the buffer to a multi-block file
via `WriteNew`/`WriteAt`. The remaining hard cap is the 4 KiB *service*-stage message.)

So a *builtin-only* capture can fill 64 KiB, but it can only reach a sink that can take it -
which today nothing beyond 4 KiB can. Lifting this is the streaming work, not a constant.

## Future: true streaming (design intent - not built)

A streaming pipeline would match the POSIX mental model - `a | b | c` running **concurrently**,
bytes **flowing through** bounded queues, total data unbounded. It is a deliberate future
project, not a tweak, and it is where §8.9's deadlock discipline stops being theoretical. The
pieces:

1. **Concurrent stages.** Each stage is a live task for the pipeline's duration (not spawned and
   reaped one at a time). The shell wires stage *i*'s output cap to stage *i+1*'s endpoint, so
   data flows producer→…→sink without the shell materialising any stage in full.
2. **Backpressure.** With bounded per-endpoint queues (depth 16, §8.5), a fast producer must
   block on a full queue until the consumer drains it - `send` already blocks, so the queue *is*
   the backpressure, but every stage must then use the **`try_send`/structured discipline** §8.9
   requires so a stall can't become a deadlock.
3. **Multi-block files - done.** `\| write` already streams to a multi-block file
   (`WriteNew`/`WriteAt`); a large piped capture reaches the file. What remains is *streaming* it
   chunk-by-chunk rather than materialising the whole buffer first.
4. **A streaming filter contract.** A filter service reads a chunk, emits a chunk, repeats -
   `upper` is already shaped this way (chunk-in → chunk-out); the change is the shell *not*
   draining it in full but forwarding each chunk onward.

The store-and-forward machinery here is the foundation: the producer/filter/sink roles, the EOT
marker, and the whole-buffer round-trip generalise to chunked streaming without changing the
shape - only *who drains whom, and when*.

## Filter built-ins

A **filter built-in** consumes the previous stage's buffer and emits to the next - it runs
in-process, so it is **not** subject to the 4 KiB service-boundary cap and can filter a full
64 KiB buffer. Built so far: **`match`** (grep - `utilities/27_match.md`), **`count`** (wc -
`utilities/28_count.md`), **`sort`** (`utilities/29_sort.md`), and **`first`/`last`** (head/tail -
`utilities/30_first-last.md`): `read /log | match error | sort | last 20`. Piping into command
arguments is the remaining Appendix-D work; new filters drop into the same middle FILTER slot.
