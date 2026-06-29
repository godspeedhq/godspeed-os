# examples/upper/

A pipe **filter**: receives text, re-emits it uppercased. Sits anywhere in a chain, for example
`echo hi | upper | write /f.txt`.

> Point your AI at this file for the capability-pipe **filter** pattern (a stage with both an
> input and an output). The "why" sections are grounded in the Ten Commandments
> (`COMMANDMENTS.md`).

## Purpose

Show a middle pipe stage: read the previous stage's output, transform it, send it on. A filter
needs no knowledge of *who* feeds it or *where* its output goes; the shell brokers both ends.
That is composition without coupling.

## What it demonstrates

- The input side: `ctx.recv()` on the filter's own endpoint. The shell resolves "upper" by name
  through the kernel name directory and routes the upstream stage's messages here (Path C: no
  self-registration; the kernel records the name at spawn).
- The transform: uppercase each ASCII byte into a fixed `[u8; 4096]` buffer (one message's worth).
- The output side: send the result over `send_peers[0]` - the SEND cap the shell delegated at
  spawn (`ctx.send_peer_at(0)`), the same mechanism `greet` uses.
- The protocol: forward a lone EOT (`0x04`) downstream so the shell stops draining this stream.

## Why it is built this way (the Commandments)

- **Commandment VI (no shared mutable state).** Both ends are IPC: input arrives as messages on
  an endpoint, output leaves over a cap. There is no shared stream object between stages, so each
  stage is isolated and restartable on its own.
- **Commandment VII (no ambient authority).** `upper` cannot reach its downstream sink except
  through the SEND cap the shell delegated. It holds no standing authority to send anywhere, and
  the upstream side reaches it only because the shell routed to its name. Authority is brokered.
- **Commandment X (complexity in the right layer).** A filter is pure data transformation; the
  shell owns the wiring. The kernel provides only the IPC mechanism (§8). Putting the routing in
  the shell keeps both the kernel and the utility simple.
- **Commandment II (love Chaos).** A filter killed mid-chain respawns fresh; the shell re-wires it.

## The contract, annotated

`upper` has a **minimal contract** (`examples/upper/contracts/upper.toml`) declaring **only
`log_write` and no send peers** (no `ipc_send`). Its input endpoint is named by the kernel name
directory at spawn, so it declares no `ipc_receive` either; the output SEND cap is delegated by the
shell at spawn (`send_peers[0]`, via `ctx.send_peer_at(0)`). The shell brokers **both** ends, so the
filter holds zero standing authority. Declaring a fixed downstream peer would be held authority (a
small breach of **VII**) and would freeze the stage into one position in one chain - exactly what a
composable filter must avoid. `log_write` is itself a v1 default minted to every service - the
contract lists it for clarity and consistency. Every service should have a contract (CLAUDE.md §13),
so a minimal contract is the conformant, clearer way to teach "no standing send authority" than
omitting the contract: it is *present*, and it pointedly grants nothing to send with.

## What you must NOT do

- **Do not assume a global `stdin`/`stdout`.** Read from your endpoint with `ctx.recv()`; write to
  `ctx.send_peer_at(0)`. There are no inherited streams (breaks **VI**/**VII**).
- **Do not forward without honoring EOT.** Pass the `0x04` marker on, or downstream sinks hang.
- **Do not grow the buffer unbounded.** One message is at most `MAX_PAYLOAD` (4 KiB); a larger
  transform must chunk, never allocate without bound (bounded behaviour, §26.6).

## How to adapt this

To write your own filter (grep, tr, a decoder): loop on `ctx.recv()`, transform into a fixed
buffer, send over `ctx.send_peer_at(0)`, and forward the EOT byte unchanged. Keep it `no_std` and
bounded. Declare only `log_write`; let the shell broker both ends.

## See also

- `COMMANDMENTS.md` - VI, VII, X (and II).
- `docs/pipes.md` - filter stages and the EOT marker.
- `examples/greet/` (the producer that feeds a filter) and `examples/roster/` (a record producer).
- `CLAUDE.md` §8 (IPC), Appendix B.3 / D (capability-mediated pipes).
