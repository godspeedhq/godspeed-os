# docs/

Narrative documentation. These files explain design decisions in prose; they do not define policy (the spec in `CLAUDE.md` does that).

## Files

| File               | Contents |
|--------------------|----------|
| `bootstrap.md`     | Detailed walkthrough of §11: BSP init, AP startup, trampoline, failure modes |
| `ipc.md`           | IPC deep-dive: queue discipline, cross-core flow, deadlock patterns, examples |
| `capability.md`    | Capability model: generation mechanism, rights model, transfer protocol, examples |
| `restart.md`       | Service restart flow: cap rebinding, core reassignment, client recovery pattern |
| `smp.md`           | SMP design: per-core queues, IPI vectors, TLB shootdown, placement algorithm |
| `unsafe-audit.md`  | Complete inventory of every `unsafe` block in the kernel (§18.4) |
| `cluster-design.md` | Cluster mode design notes: routing, API choice, failure semantics, transport, registry, TCB authority (non-normative, far-future; expands Appendix C.4) |

## `unsafe-audit.md` is special

CI checks that `unsafe-audit.md` lists every `unsafe` block in `kernel/src/`. If you add an `unsafe` block, you must update this file in the same commit. If you forget, CI fails. This is the enforcement mechanism for §18.4.

## These docs trail the spec

When the spec (`CLAUDE.md`) and a doc disagree, the spec wins. Docs are updated as a courtesy to readers; the spec is the authority.
