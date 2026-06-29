# GodspeedOS Milestones

The master index of what has been built, in order, with a green tick on every completed milestone.
GodspeedOS is a deliberately small, fully-understood capability microkernel; the constitution is in
the repo-root `CLAUDE.md`. Each row below links to a detailed milestone write-up.

> ✅ = done and, unless noted, hardware-proven on the HP T630 (AMD GX-420GI, 4-core, real AHCI SSD).

---

## v1: the capability microkernel (✅ complete, tagged `v1.0`)

Boot multi-core; run two services; pass a message over cross-core IPC; kill one; the supervisor
restarts it (possibly on a different core); the system keeps running. Every §23 acceptance criterion
met. Summary: [`v1/_V1_MILESTONE.md`](v1/_V1_MILESTONE.md).

| # | Milestone | Status |
|---|-----------|:------:|
| 0 | [Scaffold and build infrastructure](v1/0_SCAFFOLD_AND_BUILD.md) | ✅ |
| 1 | [Boot visibility](v1/1_BOOT_VISIBILITY.md) | ✅ |
| 2 | [Memory management](v1/2_MEMORY.md) | ✅ |
| 3 | [Scheduler (single core)](v1/3_SCHEDULER.md) | ✅ |
| 4 | [Capability system](v1/4_CAPABILITIES.md) | ✅ |
| 5 | [IPC (same core)](v1/5_IPC.md) | ✅ |
| 6 | [SMP and cross-core IPC](v1/6_SMP.md) | ✅ |
| 7 | [Services and supervisor restart](v1/7_SERVICES.md) | ✅ |
| 8 | [Identity test suite (§22)](v1/8_IDENTITY_TESTS.md) | ✅ |
| 9 | [Property test suite](v1/9_PROPERTY_TESTS.md) | ✅ |
| 10 | [Fuzz tests](v1/10_FUZZ_TESTS.md) | ✅ |
| 11 | [Stress tests](v1/11_STRESS_TESTS.md) | ✅ |
| 12 | [Performance benchmarks](v1/12_PERFORMANCE_TESTS.md) | ✅ |
| 13 | [Adversarial / red-team](v1/13_ADVERSARIAL_TESTS.md) | ✅ |
| 14 | [Chaos tests](v1/14_CHAOS_TESTS.md) | ✅ |

**Post-v1 verification** (the test apparatus and CI, items 1 to 11): code coverage, unsafe-audit CI,
static analysis, mutation testing, property and subsystem-property tests, identity CI green (20/20),
and interrupt routing to userspace plus its tests. All ✅. Folder: [`v1/post_v1/`](v1/post_v1/).

---

## v2: userspace (ring-3) on AMD hardware (✅)

Bare-metal ring-3 bring-up on the T630. Three distinct AMD-only root causes, each producing the same
"no userspace ever runs" symptom, isolated and fixed: the SYSRETQ SS RPL bug (enter ring-3 via an
explicit IRETQ frame), the `syscall`/`int N` stall (use `ud2`/#UD as the syscall mechanism), and the
APIC-timer-ISR cascade that starved ring-3 of every instruction (resize the periodic count). Outcome:
full multi-core boot to steady state, cross-core ping/pong, and the shell prompt live on hardware.

- [`v2/MILESTONE.md`](v2/MILESTONE.md): the AMD bring-up, the diagnostic methodology, Bug 2 fix, perf.
- [`v2/STATIC_ANALYSIS_AUDIT.md`](v2/STATIC_ANALYSIS_AUDIT.md): the static-analysis + unsafe-audit pass.

---

## post-v2: a real operating system (✅)

With userspace solid, the system grew real hardware drivers, crash-consistent persistence, a security
model that confines DMA, fault-tolerance down to a single unkillable kernel, chaos-hardening, and a
usable shell with utilities. All hardware-proven on the T630.

| # | Milestone | In one line | Status |
|---|-----------|-------------|:------:|
| 1 | [Userspace drivers](post_v2/1_USERSPACE_DRIVERS.md) | Framebuffer console + USB: keyboard and mouse on both xHCI and EHCI, hot-plug, `unsafe`-free driver services via the audited SDK | ✅ |
| 2 | [Persistence](post_v2/2_PERSISTENCE.md) | AHCI block driver + the GSFS0008 filesystem: redo-journal crash-consistency, fsck/scrub, extents; `fs`/`block-driver` restartable; reboot survival on a real SSD | ✅ |
| 3 | [A file is a capability](post_v2/3_FILE_AS_CAPABILITY.md) | P2 delegated resource capabilities: a file is a genuine kernel cap, unforgeable, non-escalating, and revocable | ✅ |
| 4 | [IOMMU + DMA safety](post_v2/4_IOMMU_AND_DMA_SAFETY.md) | H1 IOMMU confinement, and the max-carnage DMA-after-free closed at three layers: contain, prevent, confine | ✅ |
| 5 | [Kernel hardening](post_v2/5_KERNEL_HARDENING.md) | W^X/NXE foundation, generation-overflow guarantee, introspection and service-control capabilities, the clean H9 syscall-surface audit | ✅ |
| 6 | [Fault tolerance](post_v2/6_FAULT_TOLERANCE.md) | Naming moved out of the kernel; registry retired, `init` removed, supervisor made restartable. The unkillable set is `{kernel}` alone | ✅ |
| 7 | [Chaos (max-carnage)](post_v2/7_CHAOS_MAX_CARNAGE.md) | A userspace resilience-stressor that sweeps every live service, and the deep-soak kernel bugs it revealed and got fixed | ✅ |
| 8 | [Shell + utilities](post_v2/8_SHELL_AND_UTILITIES.md) | The `gsh` capability-broker shell, `observe`, `edit` (any-size piece table), records, scripting + `selfcheck`, pipes, `date` | ✅ |

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
> `CLAUDE.md` Appendix C.
