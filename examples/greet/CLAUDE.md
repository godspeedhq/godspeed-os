# examples/greet/

A pipe **producer**: emits a few text lines into a capability-mediated pipe, then idles.
Run it as `greet | upper` or `greet | write /f.txt` from the shell.

> Point your AI at this file for the capability-pipe **producer** pattern. The "why" sections
> below are grounded in the Ten Commandments (`COMMANDMENTS.md`); read those too.

## Purpose

Show the smallest end of a Godspeed pipe: a service that produces output without knowing,
or caring, who consumes it. There is no `stdout`. There is only a SEND capability the shell
handed the producer at composition time.

## What it demonstrates

- A producer sends each line as an IPC `Message` over `send_peers[0]` - the SEND cap the
  **shell** delegated to it at spawn (`ctx.send_peer_at(0)`).
- It ends the stream with a one-byte EOT marker (`0x04`) so a sink knows the stream is done
  without waiting forever (a zero-length message is not a reliable signal).
- It declares **no** send peers of its own. Its only reach is the one cap the shell wired in.

## Why it is built this way (the Commandments)

- **Commandment VI (no shared mutable state).** Output flows as IPC messages over a cap, not
  through a shared buffer or a global `stdout`. The pipe is a real endpoint, so the producer
  and consumer stay isolated and independently restartable. This is also why Godspeed pipes
  are not POSIX fd inheritance: an inherited fd is shared plumbing; a delegated cap is not.
- **Commandment VII (no ambient authority).** `greet` holds zero authority to talk to anyone.
  It cannot reach the sink unless the shell *grants* it a SEND cap at spawn. Authority is
  granted at composition time, never held. Change the pipe target and `greet` is unchanged.
- **Commandment X (complexity in the right layer).** The shell is the capability broker: it
  decides what connects to what and mints the caps. The producer just produces. Routing
  policy lives in the shell, not in the kernel and not smeared across every utility.
- **Commandment II (love Chaos).** Before a producer is "done" it must survive being killed
  mid-stream by `chaos max-carnage` and respawned; a fresh instance simply re-runs.

## The contract, annotated

`greet` has a **minimal contract** (`examples/greet/contracts/greet.toml`) declaring **only
`log_write` and - the lesson - no send peers** (no `ipc_send`). A producer that declared a fixed
peer would hold standing authority to reach it (a small violation of VII). Instead the shell
delegates the SEND cap dynamically at spawn, installed as `send_peers[0]` (reached via
`ctx.send_peer_at(0)`); authority is granted at composition time, never held. `log_write` is itself
a v1 default minted to every service - the contract lists it for clarity and consistency. Every
service should have a contract (CLAUDE.md §13), so a minimal contract is the conformant, clearer way
to teach "no standing send authority": the contract is *present*, and it pointedly grants nothing to
send with.

## What you must NOT do

- **Do not assume a global `stdout`/`stdin`.** There is none (breaks **VI**/**VII**). Send only
  over the delegated cap (`ctx.send_peer_at(0)`); if it is `None`, you were not wired to a sink.
- **Do not hardcode the consumer** (for example, look up "upper" by name and send to it). That is
  held authority and invisible coupling (breaks **VII**/**VI**). Let the shell broker the link.
- **Do not skip the EOT marker.** A sink that drains until EOT would hang forever otherwise.

## How to adapt this

To write your own producer (a log tailer, a sensor reader, a generator): build each chunk as a
`Message`, send it over `ctx.send_peer_at(0)`, and finish with the `0x04` EOT byte. Keep buffers
fixed-size and `no_std` (Commandment-adjacent: bounded behaviour, CLAUDE.md §26.6). Declare only
`log_write`; let the shell grant the pipe cap.

## See also

- `COMMANDMENTS.md` - VI, VII, X (and II).
- `docs/pipes.md` - the four pipe shapes and the EOT end-of-stream marker.
- `examples/upper/` (a filter) and `examples/roster/` (a record producer).
- `CLAUDE.md` Appendix B.3 / Appendix D (shell is a capability broker, not a Unix shell), §8 (IPC).
