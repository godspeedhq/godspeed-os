# 35. Six copies of "the services", and no two of them agree

**Status: OPEN, counted rather than estimated. Nothing here is broken today; the risk is that these
lists drift apart silently, and two of them already have.**

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
