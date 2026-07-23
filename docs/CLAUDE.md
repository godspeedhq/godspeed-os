# docs/

Narrative documentation. These files explain design decisions in prose; they do not define policy (the spec in `CLAUDE.md` does that). When this directory and `CLAUDE.md` disagree, `CLAUDE.md` wins.

## Files

| File                | Contents |
|---------------------|----------|
| `bootstrap.md`      | Detailed walkthrough of §11: BSP init, AP startup, real-mode trampoline, failure modes |
| `ipc.md`            | IPC deep-dive: queue discipline, cross-core send flow, deadlock patterns, examples |
| `capability.md`     | Capability model: generation mechanism, rights model, transfer protocol, lifecycle examples |
| `restart.md`        | Service restart flow: cap rebinding, core reassignment, client recovery pattern |
| `pipes.md`          | Composing built-ins and services with `A \| B`: capability-mediated pipes (not POSIX fd inheritance), the four shapes (builtin/service × write/service), directory-resolved sinks, the EOT end-of-stream marker (Appendix D.3) |
| `smp.md`            | SMP design: per-core run queues, IPI vectors, TLB shootdown protocol, placement algorithm |
| `iommu.md`          | IOMMU-backed DMA confinement (H1): why DMA-capable drivers are kernel-equivalent without an IOMMU, AMD-Vi detection/setup/confinement/reclaim, Phase 2 TCB-drop proposal (§6, §12, §18.1) |
| `persistence.md`    | Block driver + filesystem (v2): why our own filesystem not ext4/btrfs, ATA PIO (no-DMA, least-privilege), flat name→blob format, file-as-capability via kernel-delegated resource caps, phased plan + TCB-drop trajectory (§6.3, §15, §23.4) |
| `ahci.md`           | AHCI (SATA) block-driver backend: why (T630 SSD is AHCI-only), MMIO+DMA shape, command list/FIS/PRDT, IOMMU confinement (H1), incremental build steps A-E |
| `usb-hub.md`        | xHCI USB hub enumeration (design + in-progress, Wyse 5070 bring-up): why the back-port keyboard behind a Realtek hub is invisible, the EHCI split-transaction reference vs xHCI's route-string/topology-aware slot contexts, the `enumerate_one` refactor + recursive hub walk (§12) |
| `drives.md`         | `drives` multi-drive model + rationale (design, not built): addressing `[N:]label/path`, default-drive superblock flag, drive labels = identity over location (invariant 11), no-reboot flow. **Command surface is `utilities/15_drives.md` + `16`-`24`** (this doc's verb list is superseded - `mount`/`use` were dropped) |
| `prime.md`          | GodspeedOS Prime (design, not built): the minimal self-installing portable core (TCB + run/portability utilities), bootable-drive anatomy (ESP boot region + GSFS), `flash`/`install`/`update`, self-install USB→SSD + self-replication, A/B kernel self-update (§16 generalized), carrying a "world" on a drive |
| `licensing.md`      | Licensing intent/policy (not yet legal text): GPL copyleft kernel + permissive SDK, the capability/IPC boundary as the license boundary, Limine BSD-2-Clause compatibility, GPLv2-vs-v3 + MIT-vs-Apache open choices |
| `unsafe-audit.md`   | Complete inventory of every `unsafe` block in the kernel (§18.4) |
| `kernel-audit.md`   | **Living audit** of the ring-0 kernel against the invariants; north-star: nothing above the kernel may panic or wedge it |
| `userspace-audit.md`| **Living audit** of the userspace services against the Commandments; north-star: identity over location, wait on truth incl. failure, reacquire + retry |
| `documentation-audit.md` | **Living audit** of the *documentation* for clarity and intent - the least-capable model should not have to guess. Third of the audit family; maintains `anti-patterns.md` |
| `security-audit.md` | **Living audit** of *authority* against escalation; north-star: no principal gains authority beyond its grant. Fourth of the audit family; the TCB-principal threat model (the "root" analog is reaching a deputy). Framed against the AI-vuln pattern (ksmbd/Big-Sleep style) |
| `anti-patterns.md`  | Field Guide to Constitutional Violations: 21 categories, each violation paired with the correct pattern and the Commandment/section it breaks |
| `introspection-capability.md` | Design note: gating `InspectKernel`/`TaskStat` behind the `INTROSPECT` cap (§3.1) - closes the ambient-introspection exception |
| `networking.md`     | **Networking (v2 design, not built):** network stack as a userspace service - a socket IS a capability (the same delegated-resource-cap mechanism as file-as-capability, §7.10/P2), so the kernel gains nothing (§4.4); IOMMU-confined NIC driver (e1000 for QEMU/Intel, T630 chipset TBD via a Phase-0 PCI print); ARP/IPv4/ICMP/UDP phased plan with TCP far-future; no ambient network (§3.1, Appendix D.4), §23.4 |
| `cluster-design.md` | Cluster mode design notes (non-normative, far-future; expands Appendix C.4 of `CLAUDE.md`) |
| `naming-design.md`  | **✅ Complete (Path C, §3.7):** name→endpoint *wiring* moved out of the kernel - the supervisor wires every service from a `name → cap` map at boot + restart, and the kernel keeps only a *minimal gated recovery directory* (`ipc::names` + `AcquireSendCap`). All phases done: registry service retired into the directory (Phase 4); `init` removed (Phase 5); **supervisor made restartable** (Phase 6) → unkillable = `{kernel}` only (§6.3 floor reached). Traded a sliver of §26.10 for max fault-tolerance. |
| `multi-arch.md`     | **The multi-architecture proof (2026-07-14):** the arch-neutral kernel compiled + booted on THREE ISAs - x86-64 (full OS + shell), AArch64 (QEMU virt, PL011), RISC-V (QEMU virt, OpenSBI/16550) - by writing only `arch/<isa>/`. The table + boot evidence + per-arch bring-up notes. The executable payoff of the demarcation. |
| `arm32-status.md`   | **Built + running (Raspberry Pi 2 / ARMv7-A):** the living status of the 32-bit ARM port - what runs on real hardware (kernel, supervisor, shell, ping/pong IPC, 4-core SMP), how to build + boot it (`scripts/arm_build.py` / `arm_run.py`), how to add a service, known gotchas (release-not-debug, no RTC), and the remaining hardware drivers (DWC2 keyboard in progress, SD/EMMC, LAN9514). Implementer reference: `kernel/src/arch/arm/CLAUDE.md`. |
| `aarch64.md`        | **Design, not built (Raspberry Pi 4 / AArch64):** the ARM port plan. Includes the *measured* arch-boundary punch-list (126 `arch::x86_64` refs + 23 asm sites, all in CPU plumbing; zero in capability/ipc), the "Phase 0: seal the boundary on x86 first" refactor (cfg-alias vs HAL-trait fork), the `arch/aarch64/` surface (GIC / MMU / EL0-EL1 / generic timer / PSCI / PL011), Pi 4 board specifics (GENET network-first, VL805 xHCI over PCIe = xhci reuse, EMMC, no usable SMMU so §6.4 doesn't travel), the bring-up order, and the CLAUDE.md amendments the port will need. Non-normative until those amendments land. |
| `power.md`          | **Design + active (`feat/power-efficiency`):** reducing CPU/power draw. Diagnoses the `xhci`/`ehci` busy-spin (the only services not at 0% CPU) and the battery cost; the busy-poll vs sleep-poll (rejected "cool-c1") vs interrupt-driven tradeoff; the fix = block on the controller interrupt (§12, matches Linux URB / macOS IOKit), interrupt-primary + slow-watchdog fallback + IRQ-core pinning; the next lever (tickless idle) and its tension with the liveness watchdog; efficiency-core placement as an arch-gated future (big.LITTLE ARM). Phased: (1) interrupt-driven USB, (2) tickless idle, (3) E-core placement. |

## `unsafe-audit.md` is special

CI checks that `unsafe-audit.md` lists every `unsafe` block in `kernel/src/`. If you add an `unsafe` block, you must update this file in the same commit. If you forget, CI fails. This is the enforcement mechanism for §18.4.

The current total (see the inventory table in `unsafe-audit.md` for the authoritative count) covers the 4 permitted layers (`arch/`, `memory/`, `capability/`, `smp/`) plus grandfathered lines in `task/`, `syscall/`, and `interrupt/`. The grandfathered counts are frozen - they may decrease but increase only by a recorded `CLAUDE.md §18.5` amendment with rationale.

## `cluster-design.md` is non-normative

It records design intent for a far-future multi-node capability extension. Nothing in it amends the constitution. The architectural primitives in `CLAUDE.md` (identity over location, generation-based revocation, explicit cap transfer) are unusually well-suited for multi-node extension - that's why the design notes exist - but the work is multi-year and not on any milestone timeline.

## These docs trail the spec

Docs are updated as a courtesy to readers; the spec (`CLAUDE.md`) is the authority. When implementing a feature, read the spec section first, then the doc if you want more detail. Do not treat doc and spec as equal - the spec wins on any conflict.
