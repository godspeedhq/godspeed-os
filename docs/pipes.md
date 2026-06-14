# Pipes — composing built-ins and services

> **Status:** Implemented + hardware-verified on the T630 (`osdev test files`, 6 pipe cases,
> both directions). Realises part of **Appendix D.3** (capability-mediated pipes). Trails
> `CLAUDE.md`; does not amend it.

`A | B` feeds the output of `A` into `B`. Unlike a POSIX pipe (an inherited file descriptor
across a `fork`), a GodspeedOS pipe is **capability-mediated IPC** (Appendix D.3): the shell
brokers a real endpoint cap between the two stages. There is no shared buffer and no ambient
authority — the data always crosses a real IPC boundary (§26.4 forbids a silent shell-internal
data path masquerading as a pipe).

Either side may be a **shell built-in** or a **service**, so there are four shapes:

| Shape | Example | What happens |
|-------|---------|--------------|
| `builtin \| write <file>` | `tree / \| write /snap.txt` | capture the producer's text, write it to the file |
| `builtin \| service` | `read /log \| upper` | capture it, send it to the sink service's endpoint |
| `service \| write <file>` | `greet \| write /out.txt` | drain the producer's stream to the file (shell is the sink) |
| `service \| service` | `greet \| upper` | spawn the sink, then the producer with a delegated SEND cap |

## Producers and sinks

- **Producer built-ins** (capture their output): `read`/`cat`, `echo`, `ls`, `tree`, `find`.
  They render through an `Out` target that is either the console or a bounded capture buffer
  (one message, 4 KiB; loud on overflow, §26.6). Errors always go to the console, never into
  the pipe.
- **Sink built-in**: `write <file>` — writes the piped bytes to a file (overwrites).
- **Producer / sink services**: any service. A **sink service** must register its name (e.g.
  `upper` calls `ctx.register("upper")`) so the shell can resolve its endpoint via the
  registry — the shell holds no contracted cap to it. A **producer service** drained by a
  built-in sink must follow the EOT protocol (below).

## How each shape is wired (no new syscall)

- **builtin → service.** The shell captures the producer, spawns the sink, resolves its
  endpoint with `registry_lookup` (retried while the sink registers), sends the captured text
  one line per message, then reaps the sink. The shell never held a contracted cap to the sink
  — it looked it up at runtime, like any registry client.
- **service → builtin.** The shell spawns the producer wired to its **own** endpoint
  (`spawn_pipe(producer, "shell")` — the shell is `has_recv_endpoint` and registered in the
  kernel name table). The producer sends to its delegated `send_peers[0]`; the shell drains
  with `recv` until the **EOT** marker, then runs the sink built-in on the buffer.
- **service → service.** The original demo: spawn the sink, then the producer with a SEND cap
  to the sink delegated at spawn (`greet | upper`). The producer holds no ambient authority —
  its only reach is the cap the shell granted.

## The EOT end-of-stream marker

A producer service that streams to a **built-in sink** ends its output with a one-byte **EOT**
(`0x04`) message. The shell drains `recv` until it sees EOT, so it knows the stream is done
without blocking forever. A zero-length message is **not** used — the IPC path does not deliver
an empty body. A service sink (`upper`) simply skips a lone EOT.

## Bounds and failure (loud, never silent — §26.6 / §3.12)

- Captured producer output is one 4 KiB message; overflow is reported, not silently truncated.
- `service | write` only accepts a producer that follows the EOT protocol (a small whitelist —
  currently `greet`). A non-conforming service is refused loudly rather than wedging the shell
  on a `recv` that never returns (there is no non-blocking `recv` in v1).
- A sink service that never registers is reported (`pipe: sink '<name>' never registered`).

## Not yet (Appendix D)

Multi-stage pipes (`a | b | c`), piping into command arguments, and a general filter-built-in
set (`grep`, `sort`, …) are future work. The mechanism here — capture, broker, drain — is the
foundation they build on.
