# IOMMU-backed DMA confinement (H1)

> **Status:** Phase 0 + Phase 1 (a–f) implemented and QEMU-verified on branch
> `feat/iommu-dma-confinement`. Phase 2 (dropping the drivers from the TCB) is a
> **proposal pending sign-off** — see the end of this document. Real-hardware
> (T630 / AMD GX-420GI) IVRS presence is still unconfirmed (needs a flash).

This is the narrative behind H1, the flagship trusted-base reduction. The spec
(`CLAUDE.md`) is the authority; this document explains the *why* and the *how*.

---

## 1. The problem: a DMA-capable driver is kernel-equivalent

GodspeedOS runs its device drivers in userspace (§12). A driver holds an
`hw_mmio` capability for its controller's registers and a physically-contiguous
**DMA arena** for the queue structures the controller reads and writes (command
rings, event rings, device contexts). The driver builds those structures in the
arena and hands the controller **physical addresses** to find them.

Here is the catch. Without an IOMMU, the controller's DMA engine is told a raw
physical address and writes there — *anywhere in RAM*. A driver that is buggy or
compromised can program its controller to DMA over the kernel's page tables, the
capability table, another service's memory, or the kernel image itself. Nothing
in the capability model stops it, because the capability model governs *CPU*
accesses through page tables; it does not govern a *device's* DMA.

So a userspace driver with a DMA-capable device has, in practice, the same power
as the kernel. That is precisely why `xhci` and `ehci` have been treated as
trust-critical: not because of what their *code* can do through syscalls, but
because of what their *device* can do through DMA. The capability microkernel's
central promise — "no ambient authority" — has a hole exactly the size of a DMA
engine.

## 2. The mechanism: an IOMMU translation domain per driver

An IOMMU (AMD calls it AMD-Vi) sits between devices and memory and translates
every device DMA through a per-device page table, exactly as the MMU translates
every CPU access through the process page table. If we give each DMA-capable
driver an IOMMU domain whose I/O page table maps **only its granted arena**, then
the device can reach its arena and *nothing else*. A DMA outside the arena has no
translation and faults — it does not silently corrupt memory.

With that in place, a compromised driver is confined to the same memory the
capability model already granted it. The device's authority becomes explicit and
bounded, just like everything else in the system — and the driver can be dropped
from the trusted base.

```
   without IOMMU                         with IOMMU (H1)
   ┌────────┐  phys addr                 ┌────────┐  IOVA
   │ driver │ ───────────► [ all RAM ]   │ driver │ ──► [ IOMMU ] ──► arena only
   └────────┘   (device DMA)             └────────┘      │  fault on
        can scribble the kernel               anything else ┘  everything else
```

## 3. How it is built

The implementation lives in `kernel/src/arch/x86_64/iommu.rs` (the unsafe
hardware boundary, §18.1). Every raw access carries a `// SAFETY:` argument; the
file is fully accounted in `docs/unsafe-audit.md`. The rest of the kernel touches
it only through three safe entry points: `bringup`, `confine_device`,
`release_device`.

### Phase 0 — detection (`detect`)

Before building anything, prove the hardware can do it. Limine hands us the ACPI
RSDP pointer; we walk RSDP → RSDT/XSDT → **IVRS** (the AMD-Vi description table).
If there is no IVRS, this machine has no AMD-Vi IOMMU: we say so loudly and the
drivers stay in the TCB (no behaviour change). If there is one, we record its
MMIO base. This is the gate for everything below.

### Phase 1a — MMIO bring-up (`bringup`)

Map the IOMMU's MMIO register block uncached (PCD|PWT, the same way the APIC is
mapped) and read the Extended Feature Register and control state. This proves the
kernel can talk to the IOMMU and reads the capabilities the later phases depend
on (page-table levels, current enable state).

### Phase 1b — structures (`setup_structures`)

Allocate and program the IOMMU's three core structures:

- **Device table** — one 256-bit Device Table Entry (DTE) per 16-bit PCI BDF
  (full 2 MiB table). Every entry defaults to **passthrough** (`V=1, TV=0,
  IR=1, IW=1`), so when translation is later switched on, the disk and every
  other device keep DMAing untranslated. Only the USB controllers get switched
  to a confined domain.
- **Command buffer** — the ring through which we issue cache-invalidation
  commands to the IOMMU.
- **Event log** — the ring on which the IOMMU posts translation faults.

### Phase 1c — enable (`enable_passthrough`)

Turn on the command buffer, the event log, and then the master `IommuEn` bit.
Every device DMA is now checked against the device table — but since every entry
is passthrough, nothing changes yet. This proves the translation engine runs
without breaking the running system (verified: zero fault events, disk fine).

### Phase 1d — confinement (`confine_device`)

When a driver's DMA arena is granted at spawn (`task/mod.rs`), confine it:

1. Build a private 4-level AMD-Vi I/O page table that **identity-maps** (IOVA ==
   PA) only the arena's pages, with read+write permission. Identity mapping means
   the driver keeps handing the controller the same physical addresses it always
   did — they are now IOVAs that translate back to themselves.
2. Switch the device's DTE from passthrough to that domain (`V|TV|mode=4|root`).
3. Invalidate the cached DTE and the domain's page cache through the command
   buffer, so the new entry takes effect.

The arena design from §12 is what makes this clean: because a driver's
device-visible memory is *exactly* one contiguous arena, the confining I/O page
table is tiny and the identity map is trivial.

### Phase 1e — proof (`confinement_selftest`)

A read-only walk of the freshly built I/O page table confirms every arena
boundary page translates identity and the first page *past* the arena is
unmapped. Combined with "the driver still works while confined" (its rings DMA
correctly through the domain), this is a complete confinement proof: the mapped
region is reachable, nothing outside it is.

### Phase 1f — reclaim on death (`release_device`)

For a confined driver to be **restartable**, its IOMMU resources must be
reclaimed when it dies, or a restart would leak the I/O page table and re-confine
on top of a stale domain. On driver death (`scheduler.rs` kill path) we revert
the DTE to passthrough, invalidate it, and free the I/O page-table frames (only
the table frames — the arena leaf pages return via ordinary task reclaim). A
restart then re-confines cleanly with a fresh arena.

## 4. What is verified, and where

All of Phase 0 and Phase 1 are verified under QEMU with an emulated AMD-Vi
(`-device amd-iommu`) and a `qemu-xhci` controller behind it. The reusable
launcher is `scripts/qemu_iommu.sh`. Observed end-to-end:

```
iommu: AMD-Vi IVRS found ... IOMMU MMIO base 0xfed80000
iommu: H1 Phase 1a OK -> MMIO reachable; capabilities read
iommu: H1 Phase 1b OK -> device table + rings programmed
iommu: enable -> control=0x1005 (IommuEn=1) ... zero fault events
iommu: selftest PASS — arena .../...  translate identity, ... (outside) unmapped
iommu: confined BDF 00:04.0 -> domain 1 arena ...; DTE invalidated
   (xhci driver then initialises and runs normally — confined DMA works)
control: RESTART xhci ...
iommu: released BDF 00:04.0 -> DTE back to passthrough, I/O page table freed
iommu: confined BDF 00:04.0 -> domain 1 arena <fresh>; selftest PASS
```

The negative path (no `-device amd-iommu`) prints "no IVRS ... drivers stay in
TCB" and the system boots normally — H1 is a no-op where there is no IOMMU.

**Confirmed on real hardware (T630, AMD GX-420GI):** the APU exposes AMD-Vi
(IVRS, IOMMU MMIO `0xfeb80000`, 6-level), and a **USB keyboard types into the
shell while its xHCI controller is confined to a 1 MiB arena with zero faults** —
the flagship demonstrated end-to-end on silicon — and ultimately **both USB
keyboards working at once with the IOMMU enabled: the front confined, the back in
passthrough.** Getting there surfaced four real bugs the emulator never could
(each a case of QEMU being lenient or the firmware quietly covering for the
driver):

1. **DTE permission bits in the wrong word.** `IR`/`IW` (bits 61/62) belong in
   the DTE's first 64-bit word; they were written into the second, setting
   reserved bits. QEMU ignored it; real AMD-Vi raised `ILLEGAL_DEV_TAB_ENTRY`.
2. **USB controllers still owned by firmware.** The drivers never performed the
   BIOS→OS handoff (EHCI's is in PCI config space, xHCI's in MMIO), so the
   firmware kept running the controllers' DMA out of firmware memory — invisible
   without an IOMMU, a fault storm with one. The kernel now hands off the
   controller it confines (`pci::xhci_bios_handoff`).
3. **xHCI scratchpad never allocated.** This controller reports
   `MaxScratchpadBufs=256` — it DMAs into 1 MiB of scratchpad it finds via
   `DCBAA[0]`. The driver left `DCBAA[0]=0` and leaned on firmware's scratchpad;
   once handed off it had none and devices dropped after binding. The arena grew
   to hold the 256 buffers and the driver builds the Scratchpad Buffer Array.
4. **Wrong passthrough DTE encoding.** Passthrough was written as `V=1, TV=0`.
   On real AMD-Vi that is **not** a transparent state — it broke *all* DMA for the
   one device actually exercising passthrough after enable (the firmware-co-owned
   EHCI controller), stalling the back keyboard. The canonical identity encoding
   — `V=1, TV=1, mode=0, root=0, IR=1, IW=1`, exactly Linux `amd_iommu`'s
   `PAGE_MODE_NONE` — is genuinely transparent. With it, EHCI runs untouched while
   the IOMMU is on. QEMU accepted the broken encoding; only hardware rejected it.

## 4a. Per-driver confinement: confine what can be confined, pass the rest through

Confinement is applied **per driver**. The xHCI driver is **confined**: handed off
+ scratchpad-complete, all its controller DMA inside the granted arena (a confined
keyboard types on hardware). The **EHCI** driver is left in **passthrough**: its
controller legitimately DMAs to firmware/hub regions outside any arena we could
grant (the `0xffffffc0` accesses), and it reaches a low-speed keyboard only
through a high-speed hub's transaction translator via split transactions — so a
tight arena confinement isn't applicable. But passthrough is now genuinely
transparent (bug 4), so EHCI works with the IOMMU enabled. The result on hardware:
**front xHCI keyboard confined to least privilege, back EHCI keyboard working in
transparent passthrough, simultaneously, zero faults.** A device is confined when
confinement fits its DMA shape and passed through (transparently) otherwise; the
trust posture is explicit and logged at spawn.

## 5. Phase 2 — DMA-capable drivers brought into least-privilege (ADOPTED 2026-06-12)

With confinement (1d/1e) and restartability (1f) in place, the trust argument for
`xhci`/`ehci` changed: an IOMMU-confined driver can no longer DMA outside its
arena, so its compromise no longer endangers the kernel or other services. That
removed the reason they were trust-critical and brought them into the ordinary
least-privilege model — the genuine trusted-base shrink H1 was aimed at.

**Adopted** as the constitutional amendment **CLAUDE.md §6.4** (with the §3.1
DMA-gap note and §22 Test 12), signed off 2026-06-12. The headline points the
amendment fixed:

- **§6 / §12.1:** state that DMA-capable userspace drivers are IOMMU-confined and
  therefore **not** trusted **on machines where an IOMMU is present**. Where no
  IOMMU is present, they remain trusted (loud, explicit — `detect` already
  reports which case holds at boot). This conditional is itself a worked example
  of "loud failure over silent fallback" (§3.12): the trust posture is a printed
  fact, not an assumption.
- **§22:** add an identity test for DMA confinement — confine a device to a
  domain that does *not* map some address, have the device touch it, and assert
  an `IO_PAGE_FAULT` event appears in the event log (the live negative control,
  complementing the structural self-test of Phase 1e). This needs the test
  harness to launch QEMU with `-device amd-iommu`; that harness change is part of
  the Phase 2 work.

**Open questions to resolve with sign-off:**

1. **Interrupt remapping.** Confinement here covers DMA to memory. If a driver
   ever uses MSI/MSI-X (writes to the `0xfeex_xxxx` interrupt region), that write
   is also a DMA and would need either an interrupt-remapping table entry or an
   explicit mapping. The current USB drivers use pin-based IRQs (no MSI), so this
   does not arise today, but the amendment should state the boundary.
2. **The no-IOMMU machine.** The conditional trust posture above means the TCB is
   *machine-dependent*. That is honest but novel for this project; it deserves a
   deliberate decision rather than a default.
3. **block-driver / fs.** These remain trusted for v1 for reasons unrelated to
   DMA (they own persistent state). H1 does not change that; the v2 plan in §6.3
   still stands.

Until sign-off, the drivers remain in their current trust posture and H1 is a
pure hardening mechanism that confines them without yet re-classifying them.
