# AArch64 Port (Raspberry Pi 4) - Design and Plan

> **Status:** design, not built. Non-normative until the constitution is amended (see
> "Constitution amendments needed" below). Target board: **Raspberry Pi 4 Model B, 4 GB, run in
> AArch64 (64-bit).** This doc captures the bring-up plan and, more importantly, the *measured*
> arch-boundary punch-list that makes the port bounded work rather than a guess.

## 1. Why the port is bounded (measured, not asserted)

The whole bet is that the microkernel isolates hardware to `kernel/src/arch/x86_64/`, so the
arch-neutral majority (capabilities, IPC, scheduler logic, services, SDK, tooling, tests) carries over
unchanged. That was audited before the port precisely so any AArch64 failure is unambiguously an
arch-layer bug, not a pre-existing one.

A static survey of the current tree (`grep` for `arch::x86_64::` and inline asm outside the arch dir)
measures how sealed the boundary actually is:

- **126** direct `arch::x86_64::` references across **16** arch-neutral files.
- **23** inline-asm sites outside `arch/x86_64/`.
- **Zero** arch references in `capability/`; **zero in code** in `ipc/` (two doc-comments only).

The verdict: the two most constitutional subsystems - the capability table and IPC - are completely
arch-clean, exactly where the "business as usual above arch" thesis has to hold. Every leak lives in
the **CPU plumbing**, which *is* the hardware interface and was always going to be rewritten:

| Area | `arch::x86_64` refs | asm sites | Nature |
|------|--------------------:|----------:|--------|
| `task/scheduler.rs`   | 34 | 9 | context switch, per-cpu, timer, halt |
| `syscall/dispatch.rs` | 31 | 0 | user-copy, `read_cycle_counter` (TSC) |
| `main.rs`             | 20 | 1 | boot orchestration (BootInfo/init/ap_init) |
| `task/mod.rs`         | 11 | 0 | spawn plumbing |
| `smp/*`               | 12 | 12 | CR3 read/write, `invlpg`, `pushfq;cli`/`popfq` |
| `memory/*`            |  5 | 1 | CR3 read (allocator) |
| `loader/control/interrupt-route/log` | ~13 | 0 | page tables, serial, IOAPIC/EOI |

The **23 asm sites reduce to ~5 operations**, each with a clean AArch64 analog:

| x86 asm | Operation | AArch64 analog |
|---------|-----------|----------------|
| `mov {}, cr3` / `mov cr3, {}` | read/write page-table base | `mrs/msr TTBR0_EL1` |
| `invlpg [addr]` | invalidate one TLB entry | `TLBI VAE1` + `DSB`/`ISB` |
| `pushfq; pop; cli` | save flags + disable IRQs | `mrs {}, DAIF` + `msr DAIFSet, #2` |
| `push; popfq` | restore IRQ flags | `msr DAIF, {}` |
| (context switch reg save) | callee-saved + PC/SP/page-base | x19-x30, SP, `SPSR`/`ELR`, `TTBR0` |

## 2. Phase 0 - seal the boundary on x86 FIRST (before any ARM)

Do the de-x86-ification as a refactor on the x86 side, verified by the identity suite (24/24 = zero
behavior change), *before* writing AArch64. Then adding `arch/aarch64/` is "implement the same surface"
instead of "also patch 126 call sites while debugging on hardware you can't see." It is 100 % on the
existing x86 target, needs no Pi, and does not touch `main`.

**Design fork (pick one):**

- **cfg-module alias** - `arch/mod.rs` selects `x86_64` or `aarch64` as `imp` via
  `#[cfg(target_arch)]`; call sites become `arch::imp::...` (or a flat re-export `arch::...`). Minimal,
  boring, mechanical - a large but low-risk sweep of the 126 sites. **Recommended for v1** (§26.13:
  discipline over cleverness; smaller and boringer wins).
- **`Arch` HAL trait** - define the ~40-operation surface as a trait, one impl per arch, call through
  it. Cleaner long-term boundary, more upfront design, easier to enforce "no arch leak" (the trait *is*
  the contract). A reasonable later refinement once two arches exist to generalize from.

Either way the surface to formalize is what `arch/x86_64/mod.rs` already exports today: `init`,
`ap_init`, `init_timer`, `halt_all_cores`, `hardware_reset`, the serial/console/UART family, IPI send,
page-table ops (via `page_tables`), user-copy (`read/write_user_bytes`, `validate_user_ptr`),
`read_cycle_counter`, and the CR3/TLB/IRQ-flag primitives currently inlined in `smp/` and `memory/`.

## 3. The AArch64 arch layer (`arch/aarch64/`, what Phase 1+ implements)

Mapped from the x86 surface, in dependency order:

1. **Boot + early init.** Entry at the firmware's load address; set up SP, clear BSS, get RAM size +
   framebuffer. Two boot-path options (section 5).
2. **MMU.** AArch64 translation tables: `TTBR0_EL1`/`TTBR1_EL1` split, the VMSAv8-64 descriptor format
   (different bits than x86 PTEs), memory attributes via `MAIR_EL1`, granule/size via `TCR_EL1`, ASIDs.
   TLB maintenance is `TLBI` + `DSB ISB` barriers, and it **broadcasts** across cores - which
   *simplifies* the shootdown path (often no IPI needed vs the x86 IPI shootdown). W^X and the
   kstack-guard map cleanly onto the descriptor AP/UXN/PXN bits.
3. **Exceptions + syscalls.** A single vector table at `VBAR_EL1` (16 entries: sync/IRQ/FIQ/SError x
   current/lower EL x width). Syscalls are the `SVC` instruction -> a synchronous exception. **This is
   where the recent C1/C2/K3/A14 hardening has its twin:** "ring-3 fault kills the task, ring-0 halts"
   becomes "was the exception from EL0 or EL1" (read `SPSR_EL1.M`). Re-establish - do not re-audit - the
   fault-kills-the-task-not-the-kernel invariant in the AArch64 sync-exception handler.
4. **Context switch.** Save/restore x19-x30, SP, `ELR_EL1`/`SPSR_EL1`, `TTBR0_EL1` (the address space);
   FP/SIMD state if used. The naked-fn shape carries; the register set changes.
5. **Interrupt controller: GIC-400 (GICv2 on the Pi 4).** Distributor + CPU interface; IPIs are
   **SGIs**. Replaces LAPIC/IOAPIC + the ICR-based IPI. More standard than the older Pi's BCM controller.
6. **Timer: the ARM generic timer.** `CNTFRQ_EL0` gives a known frequency, `CNTP_TVAL`/`CNTP_CTL` drive
   the tick. This *removes* the x86 TSC-calibration pain (the AMD `CPUID 0x15/0x16` mess on the T630).
7. **UART: PL011** (the Pi's primary UART). Small MMIO backend for `serial_write_byte` and RX.
8. **SMP bring-up: PSCI** (`CPU_ON` via `SMC`/`HVC`) on the Pi 4 firmware, or the spin-table fallback.
   Replaces the x86 real-mode INIT+SIPI trampoline (cleaner - no real-mode).

## 4. Board specifics - Raspberry Pi 4 Model B (BCM2711)

Confirm the physical board first: **Pi 4** = two micro-HDMI, USB-C power, 2xUSB3 + 2xUSB2. (There is no
4 GB Pi 3 - a 4 GB board is a Pi 4.)

- **Peripheral base `0xFE000000`** (BCM2711 low-peripheral mode); be aware of low- vs high-peripheral
  addressing.
- **GIC-400 (GICv2)** - a standard GIC, unlike the older Pi's bespoke BCM interrupt controller.
- **Ethernet = GENET**, a **memory-mapped** gigabit NIC (not USB-attached). So **net-stack does NOT
  gate on USB** - bring the network up first, independently. GENET is a new userspace driver (not
  RTL8168/e1000), but it is MMIO + DMA rings, the shape `nic-driver` already knows.
- **USB3 = a VL805 xHCI behind the BCM2711 PCIe.** Bring up a **PCIe controller** first (new), then
  **the existing `xhci` driver has a real shot at porting** - it is spec-based (drove QEMU qemu-xhci and
  the T630 controller). That replaces "write DWC2 from scratch" (the older Pi's long pole) with "PCIe +
  reuse xhci."
- **Storage = SD/EMMC** (no SATA/AHCI). `block-driver` becomes an EMMC driver, or USB mass storage once
  xhci is up.
- **4 GB + DMA ranges.** Some legacy peripherals can only DMA into the low 1 GB (bus addresses), so
  their DMA arenas must live in low memory. Fits the existing "reserved DMA arena per driver" model -
  just constrain where the arena is allocated.
- **No usable SMMU for these peripherals**, so **H1/§6.4 does not travel**: DMA-capable drivers go back
  to trusted-on-this-machine, announced loudly at boot (the machine-dependent posture the spec already
  allows). The same binary is least-privilege where an IOMMU confines it and trust-critical where none
  does - now literally true across x86-with-IOMMU and this Pi.

## 5. Boot path decision (open)

- **UEFI + Limine-aarch64.** The Pi 4 UEFI firmware (TianoCore) is mature. Keeps the handoff shape
  **identical to x86** - memory map, framebuffer, SMP topology handed over, minimal new parsing.
  Preserves the "arch layer is a reimplementation, not a new world" framing. Slightly off the stock Pi
  path (requires the RPi4 UEFI firmware on the SD card).
- **Bare GPU bootloader + DTB.** Stock Pi path: the VideoCore firmware loads `kernel8.img` and jumps to
  `0x80000` with the DTB pointer in `x0`. You get RAM size + framebuffer from the **VideoCore mailbox
  property interface** and hardcode the single known peripheral base - so full Device-Tree parsing can
  be deferred. No Limine dependency.

**Lean: UEFI + Limine-aarch64 if the firmware cooperates**, to keep the handoff identical to x86.

## 6. Bring-up order

1. Boot handoff (UEFI+Limine or GPU+DTB) -> reach `kernel_main` with a memory map.
2. GIC + generic timer + MMU + EL0/EL1 exceptions + PL011 UART.
3. SMP via PSCI (all 4 A72 cores ready).
4. **Identity suite green on the arch core** - this is the definition of "the port is done", because
   everything the 24 tests exercise above the arch line is already-hardened code.
5. Drivers, in this order: **GENET (network first, USB-independent)** -> **PCIe** -> **xhci reuse** ->
   **EMMC**.

## 7. Constitution amendments needed (before this is normative)

The spec is written single-arch in a few places; adding AArch64 turns these into "on x86 ...; on
AArch64 the analog is ...", with the rationale in the commit (§21):

- **§11.2 / Appendix A** - the Limine + real-mode INIT+SIPI trampoline is x86-specific; AArch64 uses
  PSCI/spin-table and (optionally) Limine-aarch64.
- **§6.4 (H1 IOMMU)** - AMD-Vi is x86-specific; on the Pi 4 there is no usable SMMU, so DMA drivers are
  trusted-on-this-machine (the machine-dependent posture already generalizes).
- **§9 / §10 arch notes** - CR3->TTBR, IPI-shootdown -> broadcast TLBI, ring 0/3 -> EL0/EL1.

## 8. What is NOT re-audited

The point of the pre-port audits: the arch-neutral layers do not get re-audited per arch. When the 24
identity tests pass on the Pi 4, the capability model, IPC, restartability, and every service's business
logic are the same code that already passed on x86 and hardware-soaked on the T630. The port's risk is
entirely in the arch layer and the new board drivers - which is where this plan concentrates the effort.
