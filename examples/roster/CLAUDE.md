# examples/roster/

A pipe **record producer**: emits a typed `Table` (not text) over the binary wire codec, so the
shell's record verbs operate on real fields with no JSON round-trip. Run `roster | where role=core`
or `roster | sort reverse seat | select name seat`.

> Point your AI at this file for the **structured-record producer** pattern. The "why" sections
> are grounded in the Ten Commandments (`COMMANDMENTS.md`).

## Purpose

Show that a Godspeed pipe carries *typed records*, not just bytes. Where `greet` emits text lines,
`roster` builds a typed `Table` and serializes it with the SDK's binary codec; the shell decodes
the stream straight back into a `Table`, so `where`/`select`/`sort` work on a genuine field set.

## What it demonstrates

- Build a typed table with the SDK (`godspeed_sdk::record`): `Table::new(&["name","role","seat"])`,
  `t.intern(bytes)` for string cells, `Value::Int` for numeric cells, `t.add_row(...)`.
- Serialize with the **binary wire codec** (`Table::encode` into a `RecordSink`) - the `Table`
  itself on the wire, compact and typed, not JSON. A fixed `[u8; 1024]` `BufSink` holds it
  (overflow is flagged, never silent: bounded behaviour).
- Send the encoded bytes over `send_peers[0]` (the shell-delegated SEND cap), then the EOT marker.
- The shell knows `roster` is a record service and `Table::decode`s the stream, so the record verbs
  run with no `from json` in sight. `| to json` is available at the edge if you want text.

## Why it is built this way (the Commandments)

- **Commandment III (do not duplicate truth).** The `Table` is the one source of the data; JSON,
  YAML, and the grid are **derived views** rendered from it (`to_json`/`to_grid`). The codec sends
  the source, not a flattened copy that could drift. One truth, many renderings.
- **Commandment VI (no shared mutable state).** The records travel as IPC messages over a cap, not
  a shared structure. Producer and consumer stay isolated.
- **Commandment VII (no ambient authority).** Like `greet`, `roster` declares no send peers; its
  only reach is the SEND cap the shell delegated at spawn. Authority is granted at composition time.
- **Commandment X (complexity in the right layer).** Record typing/filtering lives in the SDK
  (`record.rs`), shared by any service, with **no new kernel surface** - the kernel still just
  moves bytes (§4.4). The shell brokers the pipe. Each layer owns exactly its part.
- **Commandment II (love Chaos).** A record producer killed mid-encode respawns and re-emits.

## The contract, annotated

`roster` has a **minimal contract** (`examples/roster/contracts/roster.toml`) declaring **only
`log_write` and no send peers** (no `ipc_send`): zero standing authority to send. The SEND cap to the
sink is delegated by the shell at spawn (`send_peers[0]`, via `ctx.send_peer_at(0)`) - authority
granted at composition time, never held (Commandment VII). The record machinery needs **no** extra
capability and **no** kernel change - it is pure SDK, so a record producer is no more privileged than
`greet`. `log_write` is itself a v1 default minted to every service - the contract lists it for
clarity and consistency. Every service should have a contract (CLAUDE.md §13), so a minimal contract
is the conformant, clearer way to teach "no standing send authority" than omitting the contract: it
is *present*, and it pointedly grants nothing to send with.

## What you must NOT do

- **Do not emit JSON for service-to-service transport.** Use `Table::encode` (the binary codec);
  the shell decodes it directly. Reserve `to_json` for the human-facing edge. (Sending JSON between
  stages forces a needless parse and invites a second, drifting representation - against **III**.)
- **Do not assume a global output.** Send only over `ctx.send_peer_at(0)`; finish with `0x04` EOT.
- **Do not let the encode buffer overflow silently.** Flag it (the `BufSink.overflow` flag), or
  chunk a larger table across messages - never a lone `0x04` (that is the EOT marker).

## How to adapt this

To make your own service a first-class record producer (a process roster, a metrics table, an
inventory): build a `Table`, `encode` it into a bounded sink, send over the delegated cap, finish
with EOT. The shell's `where`/`select`/`sort`/`to json` then work on your data for free.

## See also

- `COMMANDMENTS.md` - III, VI, VII, X (and II).
- `docs/records.md` - the typed-record model; `docs/pipes.md` - the pipe transport.
- `sdk/rust/src/record.rs` - `Table`, `Value`, `RecordSink`, the codec and renderers.
- `examples/greet/` (text producer), `examples/upper/` (filter).
