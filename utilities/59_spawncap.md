<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `spawncap` - spawn a service and prove the capability to it routes

Implementation shape: **shell built-in**, asking the supervisor. A **diagnostic**, from the naming work
(`docs/naming-design.md`, Phase 0): it checks the seam the supervisor's `name -> cap` map is built on,
that a spawner is handed a capability to the service it started and that the capability reaches it.

## Status, as built and honest (2026-10-10)

Built. It answers `spawncap help` and `spawncap version` since 2026-10-10; before that it was missing
from the shell's `UTILS` list, so `version` and `help` were read as service names (`backlog/80` H8).
Its sibling `spawnwired` (spawn `greet` wired to `pong` by a passed capability) has a `help` block and
no spec of its own; it is recorded as vocabulary debt in `COMMANDMENTS.baseline.toml`.

## Usage

| Verb | Kind | What it does |
|---|---|---|
| `spawncap <svc>` | action | ask the supervisor to spawn `<svc>` and hand back a capability to its endpoint, send one byte through that capability, then give the capability back |
| `spawncap help` | | usage |
| `spawncap version` | | the version and the collective copyright line |

## What it answers

| Situation | Answer |
|---|---|
| The capability routes | `spawncap: pong - endpoint cap acquired; send Ok` |
| The send through it fails | `spawncap: pong - cap acquired but send failed`, and an error |
| The service has no endpoint to hand back | `spawncap: pong - spawned, but it has no recv endpoint to hand back`, and an error |
| The supervisor refused or the spawn failed | `spawncap: could not acquire endpoint cap for pong (...)`, and an error |
| A core service | refused, as `spawn` refuses it |

It changes state: the service it spawns stays running. It does not pipe; it is an action.
