# docs/

Narrative documentation. These files explain design decisions in prose; they do not define policy (the spec in `CLAUDE.md` does that). When this directory and `CLAUDE.md` disagree, `CLAUDE.md` wins.

## Files

| File                | Contents |
|---------------------|----------|
| `bootstrap.md`      | Detailed walkthrough of §11: BSP init, AP startup, real-mode trampoline, failure modes |
| `ipc.md`            | IPC deep-dive: queue discipline, cross-core send flow, deadlock patterns, examples |
| `capability.md`     | Capability model: generation mechanism, rights model, transfer protocol, lifecycle examples |
| `restart.md`        | Service restart flow: cap rebinding, core reassignment, client recovery pattern |
| `registry.md`       | Why the registry exists: name → capability resolution, the rendezvous problem, identity-over-location (§14.2, §3.11, §26.10) |
| `smp.md`            | SMP design: per-core run queues, IPI vectors, TLB shootdown protocol, placement algorithm |
| `iommu.md`          | IOMMU-backed DMA confinement (H1): why DMA-capable drivers are kernel-equivalent without an IOMMU, AMD-Vi detection/setup/confinement/reclaim, Phase 2 TCB-drop proposal (§6, §12, §18.1) |
| `persistence.md`    | Block driver + filesystem (v2): why our own filesystem not ext4/btrfs, ATA PIO (no-DMA, least-privilege), flat name→blob format, file-as-capability via kernel-delegated resource caps, phased plan + TCB-drop trajectory (§6.3, §15, §23.4) |
| `ahci.md`           | AHCI (SATA) block-driver backend: why (T630 SSD is AHCI-only), MMIO+DMA shape, command list/FIS/PRDT, IOMMU confinement (H1), incremental build steps A–E |
| `drives.md`         | `drives` multi-drive model + rationale (design, not built): addressing `[N:]label/path`, default-drive superblock flag, drive labels = identity over location (invariant 11), no-reboot flow. **Command surface is `utilities/15_drives.md` + `16`–`24`** (this doc's verb list is superseded — `mount`/`use` were dropped) |
| `prime.md`          | GodspeedOS Prime (design, not built): the minimal self-installing portable core (TCB + run/portability utilities), bootable-drive anatomy (ESP boot region + GSFS), `flash`/`install`/`update`, self-install USB→SSD + self-replication, A/B kernel self-update (§16 generalized), carrying a "world" on a drive |
| `licensing.md`      | Licensing intent/policy (not yet legal text): GPL copyleft kernel + permissive SDK, the capability/IPC boundary as the license boundary, Limine BSD-2-Clause compatibility, GPLv2-vs-v3 + MIT-vs-Apache open choices |
| `unsafe-audit.md`   | Complete inventory of every `unsafe` block in the kernel (§18.4) |
| `introspection-capability.md` | Design note: gating `InspectKernel`/`TaskStat` behind the `INTROSPECT` cap (§3.1) — closes the ambient-introspection exception |
| `cluster-design.md` | Cluster mode design notes (non-normative, far-future; expands Appendix C.4 of `CLAUDE.md`) |

## `unsafe-audit.md` is special

CI checks that `unsafe-audit.md` lists every `unsafe` block in `kernel/src/`. If you add an `unsafe` block, you must update this file in the same commit. If you forget, CI fails. This is the enforcement mechanism for §18.4.

The current total (see the inventory table in `unsafe-audit.md` for the authoritative count) covers the 4 permitted layers (`arch/`, `memory/`, `capability/`, `smp/`) plus grandfathered lines in `task/`, `syscall/`, and `interrupt/`. The grandfathered counts are frozen — they may decrease but increase only by a recorded `CLAUDE.md §18.5` amendment with rationale.

## `cluster-design.md` is non-normative

It records design intent for a far-future multi-node capability extension. Nothing in it amends the constitution. The architectural primitives in `CLAUDE.md` (identity over location, generation-based revocation, explicit cap transfer) are unusually well-suited for multi-node extension — that's why the design notes exist — but the work is multi-year and not on any milestone timeline.

## These docs trail the spec

Docs are updated as a courtesy to readers; the spec (`CLAUDE.md`) is the authority. When implementing a feature, read the spec section first, then the doc if you want more detail. Do not treat doc and spec as equal — the spec wins on any conflict.
