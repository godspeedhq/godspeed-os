# 35. Six copies of "the services", and no two of them agree

**Status: OPEN, but the blocking question is ANSWERED as of 2026-09-20.** Every difference between
the three shell lists has been worked through (table at the end); one was pure drift and is fixed;
two are real decisions and are named. What remains is those two decisions and the four copies
outside the shell. This entry's own enumeration of the differences was also incomplete - it missed
`time` and `ping`/`pong` entirely.

`CHAOS_RESTARTABLE`'s own comment says it is "the same fact as the supervisor's MANAGED and the
kernel's two by-name sets, stated a fourth time". Wiring deeper tab completion turned up two more,
inside the completer itself, and showed that the copies are **not** all the same list.

## The six

| where | contents | differs how |
|---|---|---|
| `supervisor` MANAGED | the services it restarts | the authority |
| kernel by-name sets (x2) | spawn + notify | build-specific extras |
| `CHAOS_RESTARTABLE` (shell) | 11 names incl. `dwc2`, `control` | the storm targets |
| `KILL_TARGETS` (shell completion) | 10 names + `all-services`, `version`, `help` | **no `dwc2`, no `control`, adds `shell`** |
| `RESTART_TARGETS` (shell completion) | 10 names | **no `dwc2`, no `control`** |
| `TARGETS` for `chaos max-carnage` | 10 names + `all-services` | **no `dwc2`, no `control`, adds `shell`** |

**`dwc2` and `control` are in `CHAOS_RESTARTABLE` and in none of the three completion lists.** So
`chaos kill-storm dwc2` is a legal command that tab completion will not offer, and
`chaos max-carnage control` likewise. The reverse also holds: `shell` is offered by two completion
lists and is not in `CHAOS_RESTARTABLE`, because killing the shell is handled by a different path.

Nothing is broken by this. A name that does not complete still works when typed, and a name offered
that the command refuses gets a clear refusal. The cost is that the completer quietly teaches a
smaller vocabulary than the command accepts.

## Why the obvious fix was not applied

Deep completion now points its `chaos kill-storm` / `flood-storm` / `max-carnage` / `trace deps` /
`trace chain` entries at `CHAOS_RESTARTABLE` rather than carrying a seventh copy. That was easy
because those five want exactly that list.

`kill`, `restart` and `max-carnage`'s target list were left alone, and deliberately. They are NOT the
same list - each differs from `CHAOS_RESTARTABLE` in a specific way, and **nobody has established
which of those differences are deliberate and which are drift.** Collapsing lists that are not the
same fact would be worse than leaving them: it would silently change what three commands offer, on
the assumption that the differences were accidents. They may be. That is the work, and it is not a
find-and-replace.

## What closing it looks like

The comment on `CHAOS_RESTARTABLE` already names the right answer and why it has not been done:

> It stays a literal for now because the shell cannot see the supervisor's list, but the honest fix
> is to derive it from live tasks the way `chaos` derives its own exclusions - which is exactly why
> chaos has no roster to drift.

So: a way for the shell to ask what services exist, rather than six hand-kept answers. `chaos`
already derives its exclusions from what it spawns, and has no roster to drift as a result - the
pattern exists in this codebase and works.

Until then, the honest intermediate step is smaller: **write down, per list, which names differ from
`CHAOS_RESTARTABLE` and why.** Four of the six differences above have no recorded rationale, so today
nobody can tell an intentional exclusion from a forgotten one - which is the actual problem, and it
survives any amount of code tidying.


---

## 2026-09-20: the differences, established

The entry said *"nobody has established which of those differences are deliberate and which are
drift"* and called that the work. It is done. First, the fact that decides how much any of it
matters:

**These lists are CONVENIENCE, not validation.** `cmd_kill` and `cmd_restart` pass any name through
to `kill_one` / `restart_one`; the only refusals are `supervisor` (for spawn/restart - `kill` is
allowed, the kernel respawns it), `shell` (special-cased into a self-kill), and the `observe`
variants. `CORE_SERVICES` is one entry long. So a name missing from a completion list costs
**discoverability, never capability** - which is what the entry already suspected and is now
checked rather than assumed.

| name | `CHAOS_RESTARTABLE` | `KILL_TARGETS` | `RESTART_TARGETS` | verdict |
|---|---|---|---|---|
| `supervisor` | yes | yes | yes | agree |
| `block-driver` `fs` `events` `xhci` `ehci` `nic-driver` `net-stack` | yes | yes | yes | agree |
| `time` | yes | **no** | **no** | **DRIFT - fixed.** A real service on every board, `kill time` always legal, and chaos storms it routinely. Nobody decided to hide it |
| `control` | yes | no | no | **DELIBERATE, and worth keeping.** It is the test harness's own channel; offering an operator a one-key path to cutting the harness out from under a running suite is not a convenience |
| `dwc2` | yes | no | no | **ARCH, and the one genuine open question.** arm32 only. Chaos lists it because chaos runs there; completion is not arch-gated, so offering it on x86 would name a service that does not exist on this machine. Offering nothing on arm32 is the other half of the same problem |
| `shell` | **no** | yes | yes | **DELIBERATE.** Killing the shell is a different path (self-kill, respawn a fresh prompt), which is exactly why chaos does not storm it |
| `ping` `pong` | no | no | yes | **DELIBERATE.** Demo services in `examples/`, spawnable and restartable, not part of the storm set |
| `all-services` | no | yes | no | **DELIBERATE.** A `kill`-only keyword; there is no "restart everything" |

### What changed

`time` is now offered by `kill` and `restart` completion. That is the only behaviour change: it was
a legal command that nothing advertised, which is the precise failure this entry describes - "the
completer quietly teaches a smaller vocabulary than the command accepts".

Nothing else was collapsed, for the reason this entry gives and which the table now supports rather
than assumes: **five of the differences are real**, so merging the lists would silently change what
three commands offer on the theory that they were accidents. They were not.

### What is still open

1. **`dwc2`.** Completion is not arch-gated and the lists are compile-time constants. Gating them on
   `target_arch` would work and would be the fifth place the shell learns which board it is on,
   which `backlog/21` and `backlog/25` are both about not doing. A device-class question the kernel
   could answer is the better shape, and that is the same answer those two entries are waiting for.
2. **`control`.** Recorded as deliberate above. If that reading is wrong it is a one-word change.
3. **The four copies outside the shell** - the supervisor's `MANAGED` and the kernel's two by-name
   sets. Untouched here; they are the authority and the spawn/notify sets, and reconciling them is a
   different question from what a completer offers.
