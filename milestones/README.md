# GodspeedOS Milestones

The master index of what has been built, organized by subsystem. GodspeedOS is a deliberately small,
fully-understood capability microkernel; the constitution is the repo-root `CLAUDE.md`, and the *why
behind the what* - the history of understanding, including the days the design overruled its author -
is in [`ALMANAC.md`](ALMANAC.md). Each row links to a detailed write-up.

> All milestones below are done and, unless noted, hardware-proven on the HP T630 (AMD GX-420GI,
> 4-core, real AHCI SSD). This folder is the "what"; the ALMANAC is the "when and why".

---

## Kernel

The microkernel itself: mechanism, not policy (memory, scheduling, IPC, capabilities, interrupts,
cross-core routing).

| Milestone | In one line |
|-----------|-------------|
| [Scaffold and build](kernel/scaffold-and-build.md) | The workspace, the `osdev` CLI, the bare-metal build target |
| [Boot visibility](kernel/boot.md) | Limine handoff, BSP + AP bring-up, the kernel ring buffer |
| [Memory management](kernel/memory.md) | Frame allocator, per-task page tables, isolation and limits |
| [Scheduler](kernel/scheduler.md) | Per-core run queues, round-robin, 10 ms preemption |
| [Capability system](kernel/capabilities.md) | Unforgeable ResourceId + Rights + Generation; the generation check |
| [IPC](kernel/ipc.md) | Synchronous, bounded-queue message passing (same core) |
| [SMP and cross-core IPC](kernel/smp.md) | Per-core scheduling, the routing table, IPI wakeups |
| [Services and supervisor restart](kernel/services.md) | Contracts, spawn, the supervisor's restart authority |
| [Interrupt routing](kernel/interrupt-routing.md) | Hardware IRQs delivered to userspace driver endpoints via IPC |
| [Interrupt routing tests](kernel/interrupt-routing-tests.md) | The tests pinning that delivery |
| [The first milestone](kernel/first-milestone.md) | §23 reached: boot multi-core, two services, cross-core IPC, kill and restart |

---

## Testing

The seven trials by fire (§22) plus the verification apparatus and CI that keep them honest.

| Milestone | In one line |
|-----------|-------------|
| [Identity](testing/identity.md) | The §22 identity suite: the executable constitution |
| [Property](testing/property.md) | P1-P10: universal invariants under randomized inputs |
| [Fuzz](testing/fuzz.md) | F1-F8: the kernel must never panic on user-controllable input |
| [Stress](testing/stress.md) | S1-S10: no drift, leak, or corruption under sustained load |
| [Performance](testing/performance.md) | B1-B10: latency and throughput baselines |
| [Adversarial](testing/adversarial.md) | A1-A10: capability isolation under direct attack |
| [Chaos](testing/chaos.md) | C1-C7: graceful degradation under partial failures |
| [Code coverage](testing/code-coverage.md) | Coverage instrumentation and reporting |
| [Unsafe audit](testing/unsafe-audit.md) | The CI check that every `unsafe` block is accounted for |
| [Static analysis](testing/static-analysis.md) | Static-analysis CI |
| [Static-analysis audit](testing/static-analysis-audit.md) | A combined static-analysis + unsafe-audit cleanup pass |
| [Mutation testing](testing/mutation-testing.md) | Mutation testing of the suite |
| [Subsystem property tests](testing/property-subsystem.md) | Property tests at the subsystem boundary |
| [Subsystem-level property tests](testing/property-subsystem-level.md) | A deeper subsystem-level property pass |
| [IPC-routing property tests](testing/property-ipc-routing.md) | P5, P8, P10 over the routing layer |
| [Additional fuzz tests](testing/fuzz-additional.md) | A second F1-F8 fuzz pass |
| [Identity CI green](testing/identity-ci.md) | The identity suite green in CI (20/20) |

---

## Hardware

Getting the kernel onto real silicon and driving real devices.

| Milestone | In one line |
|-----------|-------------|
| [Ring-3 bring-up](hardware/ring3-bringup.md) | Userspace on AMD hardware: three AMD-only root causes isolated and fixed (SYSRETQ SS RPL, the syscall/int stall, the APIC-timer cascade) |
| [Userspace drivers](hardware/userspace-drivers.md) | Framebuffer console + USB: keyboard and mouse on both xHCI and EHCI, hot-plug, `unsafe`-free driver services via the audited SDK |
| [IOMMU + DMA safety](hardware/iommu-and-dma.md) | H1 IOMMU confinement, and the max-carnage DMA-after-free closed at three layers: contain, prevent, confine |

---

## Storage

| Milestone | In one line |
|-----------|-------------|
| [Persistence](storage/persistence.md) | AHCI block driver + the GSFS0008 filesystem: redo-journal crash-consistency, fsck/scrub, extents; `fs` and `block-driver` restartable; reboot survival on a real SSD |
| [A file is a capability](storage/file-as-capability.md) | P2 delegated resource capabilities: a file is a genuine kernel cap, unforgeable, non-escalating, and revocable |

---

## Resilience

| Milestone | In one line |
|-----------|-------------|
| [Kernel hardening](resilience/kernel-hardening.md) | W^X/NXE foundation, generation-overflow guarantee, introspection and service-control capabilities, the clean H9 syscall-surface audit |
| [Fault tolerance](resilience/fault-tolerance.md) | Naming moved out of the kernel; registry retired, `init` removed, supervisor made restartable. The unkillable set is `{kernel}` alone |
| [Chaos (max-carnage)](resilience/chaos-max-carnage.md) | A userspace resilience-stressor that sweeps every live service, and the deep-soak kernel bugs it revealed and got fixed |

---

## Shell

| Milestone | In one line |
|-----------|-------------|
| [Shell + utilities](shell/shell-and-utilities.md) | The `gsh` capability-broker shell, `observe`, `edit` (any-size piece table), records, scripting + `selfcheck`, pipes, `date` |

---

## Where the system stands

- **The only unkillable component is the kernel.** Every userspace service, the supervisor included,
  is restartable; its death is recovered, never a reboot (the §6.3 TCB-shrink floor, reached).
- **Persistence is real and crash-consistent** on the T630's AHCI SSD (GSFS0008 + a redo-journal), and
  survives a power cycle.
- **A file is literally a capability** (§7.10), by the same delegated-resource-cap mechanism a socket
  will use when networking lands.
- **DMA can no longer corrupt the kernel**: IOMMU-confined where it fits, and a stray DMA is contained
  (the reclaim guard), prevented (bus-master quiesce on kill), and confined to a data frame regardless
  (the permanent DMA reserve).
- **Hardware-proven**: the T630 `selfcheck` reports `ran 163, failed 0`, and `chaos max-carnage` soaks
  to the million-round scale with zero panics.

> Forward-looking, designed but not built: networking (a socket is a capability), GodspeedOS Prime (a
> self-installing portable core), and cluster mode (`EndpointId -> (NodeId, CoreId)`). See `docs/` and
> `CLAUDE.md` Appendix C. None of this is "v-numbered" - it lands when a real need pulls it into
> existence (§26.2).
