# docs/

Narrative documentation. These files explain design decisions in prose; they do not define policy (the spec in `CLAUDE.md` does that). When this directory and `CLAUDE.md` disagree, `CLAUDE.md` wins.

## Files

| File                | Contents |
|---------------------|----------|
| `bootstrap.md`      | Detailed walkthrough of §11: BSP init, AP startup, real-mode trampoline, failure modes |
| `ipc.md`            | IPC deep-dive: queue discipline, cross-core send flow, deadlock patterns, examples |
| `capability.md`     | Capability model: generation mechanism, rights model, transfer protocol, lifecycle examples |
| `restart.md`        | Service restart flow: cap rebinding, core reassignment, client recovery pattern |
| `smp.md`            | SMP design: per-core run queues, IPI vectors, TLB shootdown protocol, placement algorithm |
| `unsafe-audit.md`   | Complete inventory of every `unsafe` block in the kernel (§18.4) |
| `cluster-design.md` | Cluster mode design notes (non-normative, far-future; expands Appendix C.4 of `CLAUDE.md`) |

## `unsafe-audit.md` is special

CI checks that `unsafe-audit.md` lists every `unsafe` block in `kernel/src/`. If you add an `unsafe` block, you must update this file in the same commit. If you forget, CI fails. This is the enforcement mechanism for §18.4.

Current floor: **52 lines** covering 4 permitted layers (`arch/`, `memory/`, `capability/`, `smp/`) plus 5 grandfathered lines in `task/`, `syscall/`, and `interrupt/`. The grandfathered count will not grow.

## `cluster-design.md` is non-normative

It records design intent for a far-future multi-node capability extension. Nothing in it amends the constitution. The architectural primitives in `CLAUDE.md` (identity over location, generation-based revocation, explicit cap transfer) are unusually well-suited for multi-node extension — that's why the design notes exist — but the work is multi-year and not on any milestone timeline.

## These docs trail the spec

Docs are updated as a courtesy to readers; the spec (`CLAUDE.md`) is the authority. When implementing a feature, read the spec section first, then the doc if you want more detail. Do not treat doc and spec as equal — the spec wins on any conflict.
