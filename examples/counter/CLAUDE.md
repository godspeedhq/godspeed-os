# Example: counter

A STATEFUL service that survives its **own** restart. Every other example is stateless - it restarts
trivially because it owns nothing to lose. `counter` owns a running count, the one thing a restart
would erase if it lived only in RAM. This example is the missing lifecycle: how a stateful service
stays restartable anyway.

## Purpose

Show, end to end, **restart-with-state**: a service that persists its state EXTERNALLY (to `fs`) and
RECONSTRUCTS it on spawn, so a kill + respawn recovers the count instead of resetting it. State
belongs to services, not the kernel (§15); a restartable service that holds state must externalize
it and reload it - that is what makes it genuinely restartable (§14, Invariant 6).

## What it demonstrates

Two halves, using only real `ServiceContext` methods:

| Half | Call | What happens |
|------|------|--------------|
| Reach `fs` | `ctx.acquire_send_cap("fs")` | resolve `fs` by name via the kernel directory; `None` -> degrade |
| **Load-on-spawn** | `ctx.request_with_reply("fs", read_op)` | read `/counter.dat` and parse the saved count (reconstruct from the durable copy) |
| **Save-on-change** | `ctx.request_with_reply("fs", write_op)` | after each increment, overwrite `/counter.dat` with the new count |
| Recover from `fs` restart | `ctx.reacquire_via_registry("fs")` | on a missed reply (cap went `EndpointDead`), reacquire by name and retry once (§14.3) |

The shape of the lifecycle:

```text
spawn ─▶ load_count() from fs ─▶ count = saved (or 0 if fs absent)
            loop: count += 1 ─▶ save_count(count) to fs ─▶ sleep
kill  ─▶ supervisor respawns ─▶ load_count() recovers the SAME count ─▶ continue
```

The fs wire protocol (`[op, path_len, path, data…]` request, `[status, …]` reply) is modelled
directly on the shell's `fs_request` / `fs_read_at` / `fs_write_new` helpers, so a real client
talks to `fs` exactly as the shell does.

## Why it is built this way (the Commandments)

- **Commandment V (no service is special - only the kernel is).** `counter` is restartable like any
  other service; its death is a supervisor restart, not a reboot. It does not assume it will keep
  running - it is built to be killed and to come back, reconstructing its state each time. If the
  Supervisor itself must survive its own death, so must a humble counter. *(COMMANDMENTS.md V;
  CLAUDE.md §6.2-§6.3, Invariant 6, Invariant 11.)*
- **Commandment IX (plan for recovery, for thy service shall fail).** RAM-only state could not
  survive a restart, so we persist it externally and reload on spawn. When `fs` itself restarts mid-
  run, our cached cap goes `EndpointDead`; we reacquire by name and retry rather than crash - the
  client's recovery obligation (§14.3). Recovery you cannot test does not exist; §22 Test 13 tests
  exactly this kind of recovery for `fs`. *(COMMANDMENTS.md IX; CLAUDE.md §14, §14.3.)*
- **Commandment VIII (wait for truth, not time).** On spawn we LOAD the real saved value from `fs`
  and parse it - we never assume the count is 0, and never infer it from how long we have run. The
  `sleep` between ticks only paces CPU; it has nothing to do with the count's correctness. Time may
  conserve CPU; it must never determine correctness. *(COMMANDMENTS.md VIII; CLAUDE.md §8.6, §9.3.)*
- **Commandment III (do not duplicate truth).** There is exactly one durable copy of the count - the
  file in `fs`. `counter` holds a working copy in RAM and reconstructs it FROM the file; the file
  wins. We never keep a second authoritative store that could diverge from it. *(COMMANDMENTS.md III;
  CLAUDE.md §15, §26.4.)*

## State belongs to services (§15)

The kernel holds no service state (§15): memory, scheduling, IPC, capabilities, interrupts, routing -
and nothing else. A service that must survive restart therefore persists OUTSIDE itself and
reconstructs on startup. `fs` is the externalization mechanism for everyone else, and `fs` itself is
restartable precisely because it gained crash-consistent recovery (it re-mounts to a consistent state
via its redo-journal, §6.8) - which is what **§22 Test 13** proves: *fs survives its own restart*,
persisted data intact. `counter` is the client-side mirror of that property: it survives its own
restart by trusting `fs` to hold the durable copy.

## The contract, annotated

```toml
[capabilities]
ipc_send    = ["fs"]        # send file-API ops (read/write /counter.dat) to fs
ipc_receive = ["counter"]   # OWNS its endpoint - fs replies here (load needs the round-trip)
log_write   = true
```

Everything the service can do is on this list and nowhere else (Commandment VII). It needs a SEND cap
to `fs` (to send ops) and its own endpoint (so `fs` can reply, via the per-request reply cap that
`request_with_reply` embeds). No `[placement]` - the supervisor round-robins it.

## What you must NOT do

- **Do not keep the count only in a `static` / in RAM and call it persisted.** A restart erases RAM;
  the count would silently reset. Persist to `fs` and reload on spawn - that is the whole point
  (**Commandment IX**).
- **Do not assume the load succeeded because `fs` is "usually up".** Check the reply status and parse
  the real value; if `fs` is absent, start at 0 and SAY SO (loud degrade, not a silent fallback -
  **Commandment VIII**, §2.4).
- **Do not treat the in-RAM count as the source of truth.** The file in `fs` is the durable copy; on
  any disagreement, the file wins (**Commandment III**).
- **Do not panic when `fs` restarts.** Reacquire its cap by name and retry; cascading recovery is the
  client's job (§14.3, **Commandment IX**).

## A compilable template

Like `examples/driver-skeleton`, `counter` is a TEMPLATE: it compiles and runs as-is, and degrades
loudly to in-RAM mode when `fs` is not reachable. To see it actually PERSIST, run it where `fs` is up
and a filesystem is mounted (`drives flash` first) - `fs` is the runnable proof. Adapt it by changing
the file path, the encoding of the state, and what triggers a save (here: every tick; equally valid:
on a received message).

## See also

- **Commandments III, V, VIII, IX** in `COMMANDMENTS.md`.
- **CLAUDE.md** §14 (service lifecycle / restart + cap rebinding), §15 (state and persistence),
  §22 Test 13 (fs survives its own restart), §6.8 (fs crash-consistent recovery).
- `services/fs` - the externalization mechanism and its crash-consistency journal.
- `services/shell` - real fs client; `fs_request` / `fs_read_at` / `fs_write_new` are the helpers
  this example is modelled on.
- `examples/ping` - restart + reacquire of a PEER service (the IPC-recovery counterpart to this
  state-recovery example).
- `examples/cap-grant` - transferring authority as a capability.
