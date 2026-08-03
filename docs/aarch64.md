# AArch64 Port (Raspberry Pi 4) - Design and Plan

> **Status:** design, not built. Non-normative until the constitution is amended (see
> "Constitution amendments needed" below). Target board: **Raspberry Pi 4 Model B, 4 GB, run in
> AArch64 (64-bit).** This doc captures the bring-up plan and, more importantly, the *measured*
> arch-boundary punch-list that makes the port bounded work rather than a guess.


> ## STATUS: milestones 1-7 done on real hardware, 8 in QEMU (2026-08-03)
>
> **GodspeedOS boots on a Raspberry Pi 4 Model B and prints over the PL011.** First AArch64 silicon.
>
> ```
> GodspeedOS aarch64: PL011 alive at EL1 (Raspberry Pi 4 / BCM2711, PL011 @ 0xFE201000)
> aarch64: neutral kernel linked; arch/aarch64 stubs pending real bodies. halting.
> ```
>
> `EL1` is the load-bearing word: the Pi's armstub hands the primary core over at **EL2**, and `_start`
> drops to EL1 before any Rust runs. At EL2 the EL1 registers it writes are settable but do not govern
> it, and FP/SIMD is gated by `CPTR_EL2` rather than `CPACR_EL1` - so Rust's NEON `memcpy` would trap
> despite `CPACR_EL1` saying otherwise. The message reports the level reached rather than merely that
> something printed, so a silent failure of the drop cannot be mistaken for success.
>
> **Boot path taken:** the stock GPU bootloader (§5's second option), not the UEFI+Limine lean. It needs
> nothing on the card but the image, and does not foreclose UEFI - that would be a different linker
> script and entry, not a change above the arch layer.
>
> **Built with `python scripts/pi4_build.py`**, which exists because the two aarch64 board variants share
> one cargo artifact path: building `virt` after `pi4` silently replaces it, and the resulting image is
> linked at `0x4008_0000` with its UART at `0x0900_0000`. That is unbootable on a Pi and looks exactly
> like a code bug. The script builds only the Pi 4 variant and **verifies `.text` is at `0x80000`**
> before emitting an image. It cost one hardware round trip to learn.
>
> **Done, each verified on the board:**
>
> | Milestone | Evidence |
> |---|---|
> | 1. Boot + UART | `PL011 alive at EL1` - the EL2 drop worked |
> | 2. MMU | `MMU ON, TTBR0_EL1=0x89000 - this line is running translated`; identity map, 4 KiB granule, 39-bit VA, 2 MiB blocks, 20 KiB of tables |
> | 3. Exception vectors | `VBAR_EL1=0x80800`, proven by a real `brk #0` reporting EC `0b111100` on vector 4 |
> | 4. GIC-400 + timer | `CNTFRQ_EL0=54000000 Hz`, 100 Hz tick, `timer IRQs DELIVERING - 10 ticks` |
> | 5. Context switch | Two kernel tasks ping-ponging; witnesses in callee-saved integer AND `d8`-`d15` verified on resume |
> | 6. EL0 + `svc` | Dropped to EL0, syscall round trip checked in both directions, clean exit; ticks 13 -> 15 across the excursion, so IRQs stayed live at EL0 |
> | 7. Memory map | `source = device tree (authoritative)`; two banks, 1968 MiB usable, the 76 MiB GPU split correctly excluded |
> | 8. Neutral frame allocator | The first arch-neutral code on this board: `crate::memory::init` unmodified, then 64 frames distinct, aligned, read-back verified, all returned (QEMU first, board pending) |
>
> The timer evidence is a rate check, not just a delivery check: the log timestamps put 10 ticks 111 ms
> apart, which is 100 Hz. A wrong reload would still have delivered ten interrupts.
>
> Also done: BCM2711 PL011 init (bounded TXFF wait, baud divisors deliberately untouched), GPIO14/15
> muxed to ALT0 with a pull-up on RX. Note BCM2711 replaced the Pi 2's `GPPUD`/`GPPUDCLK` strobe with
> direct 2-bit-per-pin registers at `0xE4` - porting the old code verbatim would compile and do nothing.
>
> **The hard-won one: an EL0-accessible region is PXN at EL1.** The kernel may not execute what
> userspace can reach - ARM's equivalent of x86 SMEP. Granting EL0 access to the 2 MiB block holding
> the kernel's `.text` therefore makes the KERNEL non-executable, and the core dies on its next
> instruction fetch with no way to report it (the handler cannot fetch its vector either). It cost two
> hardware round trips, presenting first as `tlbi vmalle1` hanging and then as the MMU enable hanging -
> the same fault landing wherever the new mapping took effect. EL0 code and stack now live in a
> linker-placed, 2 MiB-aligned `.el0` region. A real consequence: the EL0 task cannot call kernel print
> functions at all, and reports through a syscall argument instead.
>
> **Milestone 7 - the memory map, and where it comes from.** The ARM is not told how much RAM it has.
> Two sources are read, both **before the MMU and caches come on** (the GPU reads the request buffer
> straight out of RAM, so asking early removes the cache-maintenance question rather than answering it):
>
> - **The device tree**, pointer in `x0` at entry - authoritative, full 64-bit banks. `_start` was
>   throwing this pointer away: its very first instruction (`mrs x0, CurrentEL`) clobbered `x0`. It is
>   now stashed in `x19`, which survives the `eret`, and handed to Rust.
> - **The mailbox** `GET ARM MEMORY` tag - a deliberately weak fallback. It returns a 32-bit base and
>   size, so it cannot describe RAM above 4 GiB and under-reports a >1 GiB board. Which source was used
>   is **printed**, because "960 MiB" means something different depending on whether it is the machine's
>   RAM or merely all the fallback could describe.
>
> The device tree is **untrusted firmware input**, parsed accordingly: every offset bounds-checked
> against the header's own `totalsize` before it is read, the walk iteration-bounded, and anything that
> fails to parse yielding no map rather than a partially-believed one. Root `#address-cells` /
> `#size-cells` are read, not assumed - they set the width of every field in `reg`, and guessing them
> mis-parses every address on a board that differs.
>
> **The map is clamped to what the identity map actually reaches**, and that clamp is the part worth
> knowing about. The identity map covers the low 4 GiB; an 8 GiB Pi 4's firmware reports banks above it.
> Recording those as usable would hand the allocator RAM with no translation - a fault much later,
> blamed on whatever touched it. Dropping capacity is the safe direction, and the drop is reported.
> QEMU's `raspi4b` is fixed at 2 GiB, so the condition cannot be reached by configuration; the
> `memmap-clamp-test` feature injects synthetic over-limit banks and both paths were observed firing
> (an 8 GiB bank truncated, a 6 GiB bank dropped). Commandment IX - a guard never seen firing is not
> evidence that it fires.
>
> Worth knowing: the on-disk `bcm2711-rpi-4-b.dtb`'s `/memory` node is a **zero-size placeholder**
> (`reg = <0x0 0x0 0x0>`) which the firmware patches at boot, so a parser tested only against the file
> would conclude the board has no RAM.
>
> **On the board (Pi 4 Model B rev 1.5, 2 GB, BCM2711 - decoded from `board revision 0xb03115`):**
>
> ```
> mailbox: ARM memory base 0x0 size 948 MiB
> aarch64: device tree pointer (x0 at entry) = 0x2eff1c00
> memmap: source = device tree (authoritative)
> memmap:   0x0..0x80000          usable       512 KiB
> memmap:   0x80000..0x400000     kernel image 3584 KiB
> memmap:   0x400000..0x3b400000  usable       966656 KiB
> memmap:   0x40000000..0x80000000 usable      1048576 KiB
> memmap: usable RAM 1968 MiB across 4 regions
> ```
>
> **The two sources disagree by a factor of two, and that is the whole justification for parsing the
> device tree.** The mailbox reported 948 MiB; the device tree reported 1968 MiB across **two banks**.
> Taking the fallback would have cost 1020 MiB - 52% of the machine's usable RAM - and it would have
> looked entirely reasonable in the log. Note that under QEMU the two sources *agreed* (960 MiB each),
> so emulation could never have revealed this: it is exactly the class of thing only silicon shows.
>
> The 76 MiB hole between the banks (`0x3b400000..0x40000000`) is the default `gpu_mem` split, which the
> firmware excludes from `/memory` for us. Two banks also means the multi-bank path - `#address-cells=2`,
> `#size-cells=1`, several `reg` pairs in one property - is hardware-exercised rather than reasoned
> about. The clamp correctly did **not** fire: every bank on a 2 GB board is below the identity-map
> limit, which is why it needed the injected test to be proven at all.
>
> **Milestone 8 - the first arch-neutral kernel code on this board.** `crate::memory::init` is the same
> allocator the x86 build has used since v1, reached through the `BootInfo` the seam defines and
> compiled without modification. It is the demarcation claim tested on a second ISA in the place it is
> easiest to get wrong.
>
> ```
> memory: kernel phys [0x80000, 0x400000) hhdm=0x0
> allocator: frame bitmap 30 KiB x2 covers 245760 frames, carved at phys 0x3bff1000
> memory: frame allocator ready (956 MiB free)
> aarch64: frame allocator OK - 64 frames distinct, aligned, read-back verified, all returned
> ```
>
> `hhdm_offset = 0` is the **correct** value, not a missing one: the low 4 GiB is identity mapped, so a
> physical address is already addressable. The allocator distinguishes "identity" from "caller forgot"
> via `page_tables::PHYS_IS_IDENTITY`, which this port had to set `true` (it was `false`, inherited from
> the stub). Same posture as the 32-bit ARM port, opposite to x86.
>
> **The selftest writes every frame, then verifies every frame - two passes, deliberately.** A single
> write-then-read pass cannot detect **aliasing**, because two distinct physical addresses backed by the
> same RAM each read back correctly the instant after being written. Separating the passes means an
> alias overwrites the earlier frame's value and the verify catches it. That is the failure this port is
> actually exposed to: a memory map claiming RAM the board does not have shows up as address wrap or
> aliasing on real silicon far more often than as a clean fault. Proven to fire by corrupting one frame.
>
> **A latent bug found on the way in:** `serial_write_byte` and `serial_write_bytes_lockfree` - the path
> `kprintln!` uses - wrote straight to the PL011 data register with no `TXFF` poll, while the arch's own
> `put_byte` polled correctly. Harmless while this file was a boundary stub that nothing called; the
> moment neutral code began logging it would drop bytes as soon as the 32-entry FIFO filled, and it
> would have read as a kernel fault rather than a UART one. Both now route through `put_byte`, and `\n`
> is expanded to `\r\n` since the neutral log emits bare LF.
>
> **Not done:** per-task page tables, the neutral scheduler, PSCI SMP, and `kernel_main` itself. The
> remaining `arch/aarch64/` surface is still stubs. Every mechanism a scheduler needs now exists and is
> hardware-proven; what remains is wiring the neutral kernel onto them.
>
> **Known unknown:** the image that worked fixed two things at once - the link address *and* the PL011
> init. The wrong link address alone was fatal, so that was necessary; whether the firmware had already
> enabled the UART, making our init merely redundant, is untested.

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

## 1.1 The HAL contract (measured surface, categorized)

Extracting the *distinct* symbols behind those 126 references (`grep -hoE "arch::x86_64::[\w:]+"`)
gives ~90 names - but they fall into three very different buckets, and the split matters for scoping:

**(A) True arch primitives - reimplement per arch.** The irreducible hardware surface:

- **MMU:** `page_tables::{PageTable, VirtAddr, entry_for_va, unmap_4k_strided, reclaim_user_frames,
  get/set_hhdm_offset, harden_hhdm_nx}` -> VMSAv8-64 tables, `TTBR0/1`, broadcast `TLBI`.
- **Context switch:** `context_switch::TaskContext` -> x19-x30/SP/`ELR`/`SPSR`/`TTBR0`.
- **Syscall + user-copy:** `syscall_entry::{syscall_slot, USER_END, user_copy_active,
  clear_user_copy_active, init_percore_syscall_arena, init_percore_arenas}` -> `SVC` + `VBAR_EL1`,
  EL0/EL1 fault discrimination (the C1/C2/V1 twin).
- **Boot lifecycle:** `init, ap_init, ap_count, ap_boot::start_all_aps, halt_all_cores,
  hardware_reset, boot::{init_gdt_arenas, set_tss_rsp0, audit_wx}, BootInfo` -> PSCI/spin-table,
  `PSCI SYSTEM_RESET`, EL1 setup.
- **CPU + timer:** `read_cycle_counter, _rdtsc, __cpuid(_count), init_timer,
  boot::{tsc_ticks_per_quantum, TSC_DEADLINE_MODE, rearm_tsc_deadline, get_lapic_id,
  get_apic_virt_base}` -> `CNTPCT`/`CNTFRQ`, `MIDR`/ID regs, generic-timer compare.
- **IRQ flags + controller:** `disable_interrupts, wait_for_interrupt, interrupts::{send_eoi,
  idle_can_halt, fire_test_irq}, ioapic::{init, mask/unmask_vector, set_redir, set_level_route,
  set/bsp_lapic_id}` -> `DAIF`, GIC-400 distributor/CPU-interface, SGIs.
- **Serial byte in/out:** `serial_write_byte, serial_write_bytes_lockfree, com2_init,
  com2_try_read_byte, uart_rx_drain_fifo` -> PL011 MMIO. (Only the raw byte in/out is arch; see B.)

**(B) Misfiled arch-neutral logic - RELOCATE, do not reimplement.** These live in
`arch/x86_64/mod.rs` but are pure state machines with no x86 dependency beyond calling a (A) primitive:
`console_foreground_allows, claim/release_console_foreground(_if_owner), CONSOLE_READ_WAITER,
set_console_echo, console_boot_complete, console_write_bytes_gated, console_push_byte,
input_ready/set_input_ready`, and the `uart_rx_{pop, poll, drain_now}` ring buffer. Moving these to a
neutral `kernel/src/console.rs` (calling arch only for the actual byte) **shrinks the arch contract by
~15 symbols** and is a safe, compile-verifiable refactor that pays off on *every* future arch. A
genuine simplification the survey surfaced (§26.13).

**(C) Optional / board subsystems - stub or board-specific, not blocking the core:**

- `iommu::{detect, bringup, confine_device, release_device, drain_event_log}` -> **no usable SMMU on
  the Pi**, so these become no-ops and DMA drivers are trusted-on-machine (§6.4 already machine-dependent).
- `pci::{init, xhci_bios_handoff, program_xhci/ehci_msi, route_ehci_intx, ehci_flr_probe, *_BDF,
  NIC_*}` -> Pi 4 has a BCM2711 PCIe controller (for the VL805); a Pi 3 has none. Board-gated.
- `rtc::{read_datetime, now_epoch_monotonic, epoch_secs, boot_datetime, capture_boot_time}` ->
  **the Pi has no battery-backed RTC.** These degrade to "no wall clock" (date/uptime lose their RTC
  source; uptime can move to the generic timer, wall-clock date needs NTP or a DS3231 add-on later).
  A real, board-level gap to design for - not a blocker for the identity suite.
- `fb::dims_packed` -> Limine framebuffer vs the Pi VideoCore mailbox framebuffer.

**Scoping takeaway:** the true per-arch reimplementation (bucket A) is ~40 symbols in the well-known
categories above; ~15 (bucket B) are a one-time neutral relocation that helps every arch; and bucket C
is stub-or-defer. That is the real size of "supporting the architecture."

## 2. Phase 0 - seal the boundary on x86 FIRST (before any ARM)

> **Status (2026-07-14, `feat/aarch64-prep`): the seam, the bucket-A sweep, AND the asm isolation are
> DONE, and the boundary is now ENFORCED.**
> - **Seam + sweep:** `arch/mod.rs` exposes `imp` (a `#[cfg(target_arch)]` alias of the current arch
>   module); all **126** arch-neutral references swept `arch::x86_64::` -> `arch::imp::` (compiler-
>   guaranteed identical; identity 24/0).
> - **Asm isolation:** all **23** inline-asm sites in the neutral layers (`smp/`, `memory/`, `task/`,
>   `main.rs`) moved behind `arch::imp` primitives - `read/write_page_table_base` (CR3), `invalidate_tlb_page`
>   (invlpg), `local_irq_save/restore` (pushfq;cli/sti), `switch_to_boot_stack` (rsp), plus the existing
>   `enable/disable_interrupts`. The `unsafe` asm consolidated into the permitted arch layer
>   (docs/unsafe-audit.md); the host lib gets a no-op `arch::imp` stub (lib.rs). Identity 24/0.
> - **Enforcement:** `scripts/arch_boundary_check.py` (CI-wired, alongside `unsafe_check`/`contract_check`)
>   FAILS on any `asm!`/`naked_asm!`, any named-arch reference (`arch::x86_64::` etc.), OR any
>   `core::arch::<arch>::` intrinsic (e.g. `__cpuid`) outside `kernel/src/arch/`. So the demarcation
>   cannot silently rot: a future RISC-V/AArch64 port is BOUNDED by construction - implement
>   `arch/<new>/` to the `imp` surface, touch zero neutral files, and CI guarantees no neutral file
>   smuggled in arch-specific code (asm, named-arch module, or intrinsic).
> - **Clear failure for a not-yet-built arch:** `kernel/src/arch/aarch64/mod.rs` is a `#[cfg]`-gated
>   stub with a `compile_error!` that names the plan and the surface to fill. A `--target
>   aarch64-unknown-none` build fails with that message instead of a cryptic "file not found for module
>   aarch64"; it is inert on the x86 build (byte-identical binary).
>
> - **IPI-send extraction (2026-07-14):** `smp/ipi.rs` was the last file in a *permitted* layer still
>   holding APIC MMIO (the ICR programming for a targeted `send_ipi` + the shootdown broadcast). Moved to
>   `arch/x86_64/boot.rs` as `send_ipi_to_lapic(lapic_id, vector)` + `broadcast_ipi_all_but_self(vector)`;
>   `smp/ipi.rs` now resolves core->LAPIC and holds only the neutral shootdown *protocol* (per-core ack
>   masks, request/wait), calling the arch seam for the actual send. **`smp/ipi.rs` is now APIC-MMIO-free
>   - arch owns ALL hardware MMIO.** Identity 24/0 (9A cross-core IPC + the shootdown exercise the moved
>   paths). So the boundedness claim is now clean: a port reimplements `arch/`, full stop.
> - **Dash guard (2026-07-14):** `scripts/dash_check.py` (CI-wired) enforces CLAUDE.md §21 (ASCII hyphen
>   only, no em/en dash) mechanically instead of by hand-grepping each diff.
>
> **Remaining soft-spots (documented, not blocking):**
> - The IPI *vector numbers* (`WAKE_RECEIVER=0xF0`, `TLB_SHOOTDOWN=0xF1`, `SCHEDULER_TICK=0xF2` in
>   `smp/ipi::vectors`) are x86 IDT vectors passed through to `arch::imp`. A GIC port maps the IPI *kind*
>   to an SGI id (0-15), so those three numbers want to become abstract kinds that arch resolves - a
>   small change best finalized alongside the GIC impl (rule of three), not now.
> - **Bucket-B relocation** (§1.1): the misfiled arch-*neutral* console/UART state machines in
>   `arch/x86_64/mod.rs` -> a neutral module. Shrinks the arch *implementation* file; does not affect
>   boundedness (neutral either way). Pure code-motion, deferred for a live operator.

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

Either way the surface to formalize is the bucket-A list in §1.1.

**Safe execution order (each step compile- and identity-verifiable on x86, no big-bang):**

1. **Relocate bucket B** (§1.1) - move the console/foreground/echo/input-ready/UART-ring state machines
   out of `arch/x86_64/mod.rs` into a neutral `kernel/src/console.rs`, calling arch only for the raw
   byte. Shrinks the contract ~15 symbols; pure code motion, zero behavior change.
2. **Introduce the seam** - `arch/mod.rs` selects the arch impl (cfg-alias) or defines the `Arch` trait.
3. **Sweep the remaining bucket-A references** through the seam, in reviewable chunks (per subsystem:
   scheduler, syscall, smp, memory, loader), with an identity run between chunks - not one 126-site
   commit. The 23 inlined asm ops in `smp/`+`memory/` become calls to arch primitives in this step.
4. **Verify:** identity 24/24 (no behavior change) is the gate for each chunk.

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
