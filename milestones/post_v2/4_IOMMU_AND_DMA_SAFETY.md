# Milestone - IOMMU Confinement + DMA Safety ✅

**Status:** ✅ Complete - hardware-proven on the HP T630 (AMD GX-420GI). H1 IOMMU
confinement merged; the max-carnage DMA-after-free closed at three independent layers,
all on `main`.

**Target hardware:** HP T630 thin client - AMD GX-420GI, AMD-Vi IOMMU present.
**Serial capture:** COM1, 115200 8N1 → `build/putty_serial_output.log`.

---

## Scope

DMA-capable userspace drivers (`xhci`, `ehci`, `block-driver`) are the one place where a
userspace component can reach kernel-equivalent power: a device's DMA engine, programmed
with physical addresses, can read or write *anywhere* in RAM regardless of the capabilities
the driver holds. This milestone closes that hole on two fronts:

1. **H1 - IOMMU DMA confinement:** make the hardware enforce that a device can only touch
   its driver's granted arena, so a compromised driver is bounded and *leaves the TCB*.
2. **DMA-after-free defense:** a real kernel memory-safety bug that `chaos max-carnage`
   surfaced - a live controller DMA'ing into a freed-and-reused frame during kill/respawn
   churn - closed at three layers: **contain**, **prevent**, **confine**.

Both are worked examples of constitutional invariant 1 (no ambient authority) reaching the
last component that quietly violated it.

---

## Achievement 1 - H1: AMD-Vi IOMMU per-device DMA confinement

Closes the unstated exception to invariant 1 (CLAUDE.md amendment 2026-06-12, §6.4): a
DMA-capable driver had implicit kernel-equivalent reach. With an IOMMU confining the device
to its arena, a DMA outside it *faults* rather than corrupting memory, so the driver is
genuinely least-privilege and restartable - and **drops out of the TCB**. The trust posture
is **machine-dependent** and printed loudly at boot (invariant 12).

- ✅ AMD-Vi detected via the ACPI IVRS table; device table + per-device page tables set up
  in `arch/x86_64/iommu.rs`. No IVRS → loud boot fact and drivers stay trust-critical:
  `iommu: no IVRS table … → no AMD-Vi IOMMU on this machine (… drivers stay in TCB)`.
- ✅ `xhci` is confined to its DMA arena (`iommu: confined BDF … → domain … arena …`); a
  confined keyboard works on real hardware.
- ✅ `ehci` + `block-driver` run in **IOMMU passthrough** - both legitimately retain a stale
  firmware DMA pointer (~`0xffffffc0`) that survives reset (a controller quirk, not a driver
  bug); confining them would make that benign read a fatal `IO_PAGE_FAULT`. Same binary,
  different posture, the difference a printed boot fact.
- ✅ **Live `IO_PAGE_FAULT` hardware-proven on the T630** (and reproducible anywhere via the
  `iommu-fault-test` build feature, which confines a driver to an empty domain so its first
  init DMA lands out-of-arena).
- ✅ §22 **Test 12** pins the confined case: the kernel's confinement **selftest** walks the
  device's I/O page table and confirms the arena translates identity while the page one past
  `arena_end` is unmapped - the structural form of "out-of-arena DMA would fault"
  (`iommu: selftest PASS … (outside) unmapped`). QEMU's `amd-iommu` can't raise the fault
  itself, so the selftest pins the property QEMU *can* prove; the live fault is the T630's.

Full treatment: `docs/iommu.md`, CLAUDE.md §6.4.

---

## Achievement 2 - The max-carnage DMA-after-free, closed at three layers

`chaos max-carnage` (a per-round sweep that kills + respawns every live service, soaked for
hundreds of thousands of rounds on the T630) surfaced a genuine kernel memory-safety bug: a
**KERNEL PF at round 4286**, where the kill-path page-table reclaim walked a corrupted PTE
whose frame address was ~68 GB on an 8 GB box and dereferenced it through the HHDM. The root
cause turned out to be **hardware DMA**, and it is now closed at three independent layers,
all on `main`.

### Layer 1 - CONTAIN (`b9dbc4c`)

- ✅ The reclaim walk (`page_tables::walk`) validates every table and child against
  `allocator::phys_in_ram`; an out-of-RAM entry is **logged loudly and skipped** instead of
  faulting the kernel. A hard crash becomes a bounded, survivable skip - and makes the
  corruption *visible* (`page_tables: corrupt entry … outside RAM - skipped`). The guard
  fired again on the live soak (round ~29,975) and survived, proving the containment.

### Layer 2 - PREVENT, the root-cause cure (`ffe1a0f`)

- ✅ **Root cause identified as a DMA-after-free, not a software UAF.** The four obvious
  software races were ruled out by reading the defenses (zeroed tables, after-last-use frees,
  the `TASK_STATE=Dead` kill gate, idempotent double-free). The corruption is a *wild write*
  (no double-free logs fired) - a passthrough driver's controller (EHCI periodic schedule /
  in-flight AHCI command) writing into a frame freed and reused during respawn churn. The
  live log bracketed it by `ehci` re-enumeration DMA to the millisecond.
- ✅ **Cure:** quiesce the device before its frames are reclaimed. The kill path clears PCI
  **Bus-Master-Enable** for `xhci`/`ehci`/`block-driver` *before* the reclaim; spawn
  re-enables it (firmware sets BME once at boot, so a respawn must re-enable or its DMA
  silently never starts - the trap that makes the clear safe). A new `PCI_CONFIG_LOCK`
  serializes the `0xCF8/0xCFC` pair against concurrent kill/respawn.
- ✅ Boot-validated on the T630: block-driver still IDENTIFYs the SSD and `ehci` still finds
  the keyboard - BME management does not break boot-time DMA.

### Layer 3 - CONFINE (`731a939`)

- ✅ **DMA permanent-reserve:** each driver's DMA arena is allocated **once**
  (`allocator::alloc_dma_arena`) and **never returned to the general frame pool** - `free`
  skips any reserved frame, and the arena is reused across every respawn (bounded: one arena
  per driver, never a per-spawn leak). So a stray DMA can only land in DMA-reserved memory
  (corrupting only DMA data, caught by AHCI/USB CRC), **never a page table or kernel struct**.
- ✅ This closes the blind spot in Layer 1: the guard catches an *out-of-RAM* PTE but would
  follow an *in-range* garbage frame a stray DMA wrote; the reserve makes the DMA structurally
  unable to reach a page-table frame at all. Mirrors the existing `KERNEL_PT_PROTECTED` guard.
- ✅ Validated by `osdev test files` 137/0 (real AHCI: arena alloc + reuse + AHCI DMA through
  it + the `block-driver` double-storm restart); boot-validated on the T630 (arenas allocate
  + reserve cleanly, AHCI/USB DMA through them works).

> **Resolution.** Guard *contains*, cure *prevents*, reserve *confines*. The kernel-PF that
> chaos surfaced is cured at the source and bounded structurally even if a future stray DMA
> ever slips a quiesce.

---

## Commits / evidence

| Layer / feature | Commit | What |
|---|---|---|
| H1 IOMMU confinement | (branch merged) | AMD-Vi device table + per-device page tables; `xhci` confined, `ehci`/`block-driver` passthrough; live `IO_PAGE_FAULT` on T630; §22 Test 12 selftest |
| DMA contain | `b9dbc4c` | `page_tables::walk` guards out-of-RAM PTEs via `allocator::phys_in_ram`; skip + log, not fault |
| DMA prevent (cure) | `ffe1a0f` | clear PCI bus-master on DMA-driver kill before reclaim; set on spawn; `PCI_CONFIG_LOCK` |
| DMA confine (reserve) | `731a939` | `alloc_dma_arena` permanent per-driver reserve; `free` skips reserved frames; `files` 137/0 |

**Docs:** `docs/iommu.md` (H1). **Spec:** CLAUDE.md invariant 1 amendment, §6.4, §12, §22 Test 12.
**Hardware:** all layers boot-validated / fault-proven on the HP T630; serial in
`build/putty_serial_output.log`.
