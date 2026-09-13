# kernel/src/arch/

The architecture layer: the unsafe hardware boundary (§18.1), and **the single seam a new ISA drops
into**. Everything CPU-specific lives under `arch/<isa>/`; the rest of the kernel is arch-neutral and
reaches the hardware only through this directory.

If you are porting GodspeedOS to a new architecture, this file is your map. The claim it backs is the
one proven in `docs/multi-arch.md`: **a new architecture is bounded to `arch/<isa>/` - you write that
directory and nothing else in the kernel changes.** Six ISA families (x86-64, AArch64, ARMv7,
RISC-V, LoongArch, s390x) and both word sizes (64-bit and 32-bit) have been proven this way - and
**four of them boot real hardware**, not only emulation: x86-64 (HP T630, Dell Wyse), ARMv7
(Raspberry Pi 2, 2026-07-20), AArch64 (Raspberry Pi 4) and RISC-V 64 (StarFive VisionFive 2 Lite,
2026-09-07).

## The seam: `arch::imp`

`mod.rs` is the whole seam. It `#[cfg(target_arch)]`-selects one implementation module and aliases it:

```rust
#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64 as imp;      // arch::imp is now a literal alias of arch::x86_64
```

Every neutral kernel file names **`crate::arch::imp::...`**, never a specific arch like
`crate::arch::x86_64::...`. Because `imp` is an alias, routing the neutral layers through it is
behavior-identical on the current arch (the compiler resolves the same module), so adding
`arch/<new>/` that exposes the same surface is a drop-in with no neutral call site to touch.

This is not a convention you are trusted to remember: it is **mechanically enforced** (see below).

## The public surface a port must expose

`arch::imp` is expected to expose the surface the neutral kernel calls. The full x86-64 surface is
documented in `arch/x86_64/CLAUDE.md` (`BootInfo`, `init()`, `ap_init()`, `serial_write_byte()`,
`halt_all_cores()`, the safe user-pointer and cycle-counter wrappers, and so on). Match those
signatures when you bring a real port up.

If the platform has a framebuffer you want text on, the arch owes exactly ONE more thing and gets the
kernel's boot/panic blit for free: `fb_commit` (publish a written rectangle - a cache clean where the
mapping is cacheable, a store fence where it is write-combining, a drain where it is non-cacheable),
plus an init that hands `crate::bootcon` an `FbParams`: the framebuffer as a `&'static mut [u8]`, its
PHYSICAL base (what the `console` service's grant is built from), geometry, and channel shifts. That
slice is deliberate: it keeps the whole floor `unsafe`-free, so the one `unsafe` stays here in `arch/`,
where the mapping's validity is known. `arch/x86_64/fb.rs` and `arch/arm/bootcon.rs` are the templates.

**You do NOT get a terminal for that.** The ANSI/CSI parser, the UTF-8 decoder, the character grid, the
cursor and scrolling are the userspace `console` SERVICE (`docs/console-service.md` 9); the kernel keeps
only a minimal blit that draws plain ASCII and discards escapes. Where the framebuffer is ALSO granted
to that service, the arch must map it Normal **non-cacheable** to match the service's own mapping - ARM
leaves mismatched memory attributes for one physical page UNPREDICTABLE (`arch/arm/mmu.rs::section_fb`).
The AArch64 mapper collapsed `PCD || PWT` to the **Device** attribute, which is right for registers
and wrong for a framebuffer: Device-nGnRnE forbids the gathering and buffering a bulk pixel write
depends on, and one 1920x1080 repaint measured **582 ms** (~14 MB/s) on the Pi 4 - slow enough that
`selfcheck` never reached its end. A neutral `PageFlags::WRITE_COMBINE` now says what the region IS
(a framebuffer, not registers) so each arch picks its own type; AArch64 maps it Normal
non-cacheable via MAIR slot 2, and arm32 already did the right thing (`PCD|PWT` -> Normal-NC).
An arch with nothing better ignores the bit and keeps its uncached-MMIO type, which is why x86 is
unchanged. **The framebuffer is also RESERVED in the allocator** (`reserve_no_free`): it is device
memory the kernel MAPS into a service, so the kill-path reclaim would otherwise free it into the
RAM pool - see that function for the measurement that found it.

For the **first milestone** of a new arch - boot the neutral kernel and print to a UART - the surface
is far smaller: a `_start`, minimal CPU/stack setup, and a byte-out to the platform's serial device.
**Which of these are stubs, and which are finished ports, because the distinction was wrong here for
months.** This paragraph used to call `aarch64`, `riscv64` and `arm` "stubs at the first milestone".
They are not, and a porter told to copy a working 3000-line port as a "minimal template" is being
sent somewhere useless:

| directory | state |
|---|---|
| `arch/x86_64/` | complete; the reference |
| `arch/arm/` | complete, boots a Raspberry Pi 2 on real hardware |
| `arch/aarch64/` | complete, boots a Raspberry Pi 4 on real hardware |
| `arch/riscv64/` | complete, boots a StarFive VisionFive 2 Lite on real hardware |
| `arch/riscv32/`, `arch/loongarch64/`, `arch/s390x/` | **stubs** - the first milestone, and the templates to copy |

For the FIRST milestone of a new arch the surface is small, and the three stubs above are what it
looks like. The full port (MMU, exception vectors, syscalls, interrupt controller, timer, SMP, the
userspace SDK/services) is the rest of the work; `docs/aarch64.md` tracks how one of them got there.

## Adding an architecture: the checklist

> **The full map is [`docs/porting.md`](../../../docs/porting.md)** - the seam, the edges you will
> unavoidably touch OUTSIDE the kernel (the supervisor's tables, the SDK's syscall body, the service
> that picks a NIC), the four checkers that tell you where you are, and the rule: if you find
> yourself editing anything else, stop and ask why. This section is the kernel half of it.

Everything you touch in the KERNEL is in one of five places. None of them is a neutral kernel file -
which is a claim `scripts/shared_surface_check.py` now measures rather than asserts: the neutral
kernel is down to 2 arch-conditional sites, both `target_pointer_width` on one constant, and neither
is something a new port edits.

1. **`kernel/src/arch/<isa>/mod.rs`** - the implementation module. Start from the nearest existing
   stub. It begins with a `_start` (the boot handoff for your platform) and brings the CPU far enough
   to run Rust and drive a UART. This is the only directory in the kernel where new `unsafe` and
   inline `asm!` belong (§18.1).

2. **`kernel/src/arch/mod.rs`** - add the two `#[cfg(target_arch = "<isa>")]` arms (`pub mod <isa>;`
   and `pub use <isa> as imp;`). Two lines, next to the others.

3. **`kernel/kernel-<isa>.ld` + `kernel/build.rs`** - a linker script for your load address and
   PHDRS, plus a target-matching block in `build.rs` that passes `-T` for it and adds `is_<isa>` to
   `use_placeholder` (real cross-arch service ELFs do not exist yet, so the kernel embeds an empty
   placeholder - the point of the milestone is that the *neutral kernel* compiles and boots).

4. **`.cargo/config.toml`** - a `[target.<triple>]` block with the rustflags your target needs (for
   example `relocation-model=static` on the bare-metal ARM/RISC-V targets).

5. **`rust-toolchain.toml`** - add your triple to `targets` if a shipping build will need it, so a
   fresh clone can build your port without a separate install step.

   (This step used to read "extend the `_ARCHES` regex in `scripts/arch_boundary_check.py` with your
   arch name". **There is nothing to extend any more** - that list is derived from the directory
   listing of `arch/`, so the guard covers your arch the moment its directory exists. It was changed
   because the manual step had already been missed: `loongarch64` and `s390x` had directories and
   were absent from the pattern, so the check printed an unqualified all-clear it could not back.)

Then: `cargo check -p kernel --target <triple>`. Any error **outside `arch/<isa>/`** is a boundary
leak - a neutral file made an arch-specific assumption. Fix it by adding an `arch::imp` primitive, not
by editing the call site to special-case your arch. Errors *inside* `arch/<isa>/` are just your stub
being incomplete; the compiler is naming the surface you still owe.

## Two rules the boundary is built on

**No inline asm and no named-arch reference outside `arch/`.** `scripts/arch_boundary_check.py`
(a CI guard, run in `.github/workflows/build.yml`) fails the build if any kernel file outside `arch/`
contains `asm!`/`naked_asm!`, names `arch::<specific>::`, or uses `core::arch::<specific>::`
intrinsics. Arch-specific instructions live only here, reached through an `arch::imp` primitive
(`read_page_table_base`, `invalidate_tlb_page`, `local_irq_save`, ...). This is the arch-boundary
counterpart to `unsafe_check.py` (the unsafe boundary) and `contract_check.py`: the boundary survives
only because it is enforced, not because it is remembered (§26 - the architecture survives only if the
discipline survives).

**Never reach for `core::sync::atomic::AtomicU64` directly - use `portable_atomic::AtomicU64`.** This
is what makes the kernel *word-size* portable as well as ISA-portable. 32-bit RISC-V (RV32A) has no
64-bit atomic, so the `core` type does not exist there; `portable-atomic` (in `kernel/Cargo.toml`) is
the native, zero-cost `AtomicU64` on every ISA that has one and a small lock-based shim only on RV32.
Neutral code that wants a 64-bit atomic imports it from `portable_atomic`. That one dependency is the
entire cost of 32-bit support (`docs/multi-arch.md`, "Word size").

## Per-arch bring-up notes (found by actually booting)

`docs/multi-arch.md` records the gotchas a porter will otherwise rediscover the hard way:

- **AArch64** traps FP/SIMD at EL1 by default, and Rust emits NEON for `memcpy`/byte-copy, so `_start`
  must enable `CPACR_EL1.FPEN` before *any* Rust runs (found via `qemu -d int` -> ESR `0x07`). SP must
  be 16-byte aligned.
- **ARMv7 (32-bit)** is a SEPARATE port from AArch64 - modes + CP15 rather than exception levels +
  system registers, sharing no code. The Pi firmware enters it in **HYP mode** (Cortex-A7 has the virt
  extensions) whenever a device tree is loaded, so `_start` must `eret` down to SVC; `.arch_extension
  virt` is required or the assembler rejects `spsr_hyp`/`elr_hyp`/`eret`. QEMU's `raspi2b` hands over
  in SVC, so emulation never exercises that branch. Do not assume firmware initialised the PL011
  (QEMU's is disabled and silently eats output), but do NOT reprogram IBRD/FBRD - the reference clock
  differs between firmware and emulation.
- **RISC-V / LoongArch** use soft-float targets (`riscv64imac`, `-softfloat`), sidestepping the
  FP-enable step, and booted first try.
- **s390x** is **big-endian** - it compiles clean (the endian-neutrality proof) but boot is pending
  the SCLP console, which is a protocol handshake, not a register poke.
- **Boot handoff differs per platform**: x86 via Limine (higher-half), AArch64 / LoongArch via QEMU
  `-kernel`, RISC-V via OpenSBI into S-mode. Your linker script's load address follows from this.

## The SMP-port contract: memory ordering, TLB, DMA coherence (SEC-25..28)

x86-64 has a strong memory model (TSO) and cache-coherent DMA, so the neutral kernel relies on
guarantees x86 gives for free but a weaker arch (AArch64, RISC-V) does **not**. On x86 the relevant code
is correct and generates identical-or-no-op instructions; on a weak-ordered SMP port each becomes a real
race unless the port meets the obligation below. These are the security audit's **SEC-25..28**
(`audits/security-audit.md`) - **port blockers**, gathered here so a porter meets them by construction
instead of rediscovering them as heisenbugs. (They do not affect x86, so they are not "fixed" in code on
`feat/hardening`; they are specified here for whoever brings up SMP on a weak arch.)

**1. Task-slot publication ordering (SEC-25) - DONE (ARM port, kernel-audit Audit 5).** The scheduler
publishes a slot with a flag store and reads it with a flag load, then touches plain data fields
(`TASK_CTX`, `TASK_IS_USER`, `TASK_KERNEL_STACK_TOP`, ...). For the data to be visible whenever the flag
is, the *writer* stores the data **before** the flag with **Release**, and every *reader* loads the flag
with **Acquire** before touching the data. Both are now in the code:
- `reserve_task_slot` writes `TASK_CORE[i]` first, then `TASK_VALID[i] = true` (**Release**) - the flag
  publishes the data, not the reverse.
- All 34 `TASK_VALID[..].load(..)` reader sites are **Acquire**. On x86 an Acquire load / Release store is
  a plain `mov` (identical codegen); on AArch64/RISC-V/ARMv7 it emits the barrier that establishes
  happens-before. `commit_task` already publishes fields then `TASK_STATE = Ready` (Release); the SEC-1
  switch-in path is already `SeqCst`.

  Without this, a weak-arch reader could observe `VALID`/`Ready == true` with a **stale `TASK_CTX`/CR3/
  kstack** - the same use-after-free class as SEC-1. A future weak-arch port inherits the fixed ordering;
  no action needed. (The armv7 audit confirmed the *critical* scheduling path was already saved by the
  `TASK_STATE` Release/Acquire publish even before this - the residual hazard was best-effort/
  introspection readers gating a field read on a Relaxed `TASK_VALID`; those are now Acquire too.)

**2. An address-space switch must flush the TLB (SEC-26 / SEC-27).** The neutral kill path *elides* the
cross-core TLB shootdown for a pinned task ("a CR3 reload flushes non-global TLB entries"). That is an
**x86 semantic**. On AArch64 a `TTBR0_EL1`+ASID switch does not implicitly flush; RISC-V `satp` needs an
explicit `sfence.vma`. So the `arch::imp` context-switch / `write_page_table_base` primitive on a weak
arch MUST either (a) flush the outgoing address space's non-global entries on the switch, or (b) the
neutral kill path must issue the cross-core shootdown it currently elides. On the **armv7 port** the
context switch takes route (a): `switch_context` writes TTBR0 then `TLBIALL`+`dsb`+`isb` on an
address-space change, satisfying SEC-26 for the pinned single-core model. Note the arm
`invalidate_tlb_page` is **local** (`TLBIMVA`, `c8,c7,1`), *not* an inner-shareable broadcast - correct
for per-task pinned address spaces where an unmap runs on the task's own core, but a future *cross-core*
unmap that assumed a broadcast would under-flush and must upgrade to `TLBIMVAIS` (`c8,c3,1`). The neutral
`write_page_table_base` on arm does TTBR0+ISB only (no TLB maintenance); switching between private
address spaces goes exclusively through `switch_context` (which does flush), so no neutral caller relies
on `write_page_table_base` to flush. (kernel-audit Audit 5, Findings 3/4 - doc corrected to match code.)

**Every `arch::imp` primitive owes a documented SEMANTIC, not just a signature (SEC-27).** When you add
`arch/<isa>/`, treat each primitive's memory-ordering, TLB, and broadcast behaviour as part of the
contract: `write_page_table_base` flushes the old ASID's non-global TLB; `invalidate_tlb_page` covers the
VA on the required cores; the atomics keep the ordering item 1 assumes. Matching the x86 *signature* is
necessary but not sufficient - the seam pins names, and this section pins the semantics behind them.

**3. DMA cache coherence (SEC-28).** The SDK's `Dma` wrapper (`sdk/rust/src/dma.rs`) maps the arena
cacheable and does **no** cache maintenance, because "x86 DMA is cache-coherent". AArch64 (and most
non-x86) DMA is **not** coherent - CPU and device can see stale copies. A port reusing a driver there
MUST add cache maintenance (clean before a device read of a CPU-written buffer; invalidate before a CPU
read of a device-written buffer), either by mapping the arena non-cacheable or via a `dma_sync`-style
hook the accessors call. This is separate from the SMMU/H1 posture `docs/aarch64.md` already flags.

## How an arch implementation HALTS THE MACHINE, and the rules that prevent it

The section above is about being WRONG - a stale read, an under-flushed TLB, an incoherent buffer.
This one is about being SILENT, which this project ranks worse (invariant 12, and the Rule Above The
Rules: only the kernel is unkillable, so a kernel bug is the one that takes everything with it).

**`arch/<isa>/` is kernel. A mistake here does not kill a service - it kills the machine.** Every
failure below was found on the RISC-V port by running `chaos max-carnage`, and in each case nothing
above ring 0 was at fault: the arch layer was. They are gathered here because a port rediscovers them
as heisenbugs otherwise, and because three of the four are INVISIBLE - the machine does not say
anything, it just stops.

### 1. A trap handler that can fault while reporting a fault - SILENT HALT

Reporting a fault runs real code: it reads task state, walks a page table, formats, writes to a UART.
Every line of that can itself fault on the corrupt state that caused the first fault. When it does,
the trap handler re-enters, reports, faults, forever. **No output, no panic, one hart dark, nothing to
read.**

Observed exactly: a chaos run left core 0 pinned at stage `TRAP_ENTRY` with its interrupt count
frozen - the signature of an EXCEPTION loop, since an interrupt counter only counts interrupts while
the stage keeps being re-stamped. An earlier boot showed the other half directly: a kernel-mode load
page fault inside `core::fmt::write`, which is the reporting path faulting.

> **THE RULE. A fault report needs a re-entrancy guard, and the second report must say the LEAST it
> possibly can, through a LOCK-FREE writer, and then halt.** Least, because everything it might add
> is a thing that could fault: no task name, no page walk, no formatting. The fault address and cause
> of the SECOND fault are what a reader needs and they are already in hand.

Two ways to get this wrong that both LOOK right: putting the guard where it is never reached on the
faulting path, and taking the guard but never releasing it - the second turns the first real fault
into a permanent mute. `arch/riscv64/trap.rs::report_fault` is the worked example.

### 2. A wait with no bound - WEDGE

A spin on a hardware condition that never becomes true is a core that never comes back. The neutral
kernel's liveness watchdog will eventually panic on it (item 4), which is the loud outcome and the
one to want - but only if the watchdog is armed on this port.

The subtler version is a bounded wait whose RESULT NOBODY READS. The xHCI reset on RISC-V returned a
bool from `spin()` that no caller checked, so a controller that never left reset was programmed
anyway; the health line said `1 HID, disk yes` while 400 of 412 probes failed.

> **THE RULE. Every hardware wait is bounded, and every bound RETURNS A RESULT THE CALLER READS. A
> `#[must_use]` on the helper is cheap and catches it at compile time.** A count is not a duration:
> "spin 10000 times" is a different wall-clock bound on every machine, so bound on the arch's own
> time source where one exists.

### 3. Code published as DATA without an instruction-cache sync - EXECUTES GARBAGE

A loader writes a service's text through the data path. On an arch with split caches that text is not
visible to the instruction fetcher until it is made so, and on RISC-V `fence.i` is **hart-local** - so
a hart that did not run the loader can execute whatever its I-cache still holds. What it runs is a
DEAD service's text out of a recycled frame.

> **THE SIGNATURE, because it is unmistakable once seen: the same faulting PC and fault address every
> time, but only on SOME harts, and the PC disassembles mid-instruction.** The bytes being fetched are
> not the bytes in the file. Boot spawns are fine and only RESPAWNS fail, because a boot spawn gets
> fresh frames.

> **THE RULE. `finalize_service_address_space` is where a port pays this**, and it must reach every
> hart that could run the task, not just the one that built the page table. arm32 has the same
> obligation (`publish_user_pages_to_other_cores`); x86-64 does not, because its caches are coherent
> with respect to instruction fetch. Matching the x86 no-op is the mistake.

### 4. A liveness watchdog that is armed with a stubbed number - NO WEDGE DETECTION AT ALL

`ticks_before_wedge` (or whatever the port calls its quantum) gates the neutral watchdog. A stub
returning `0` does not mean "no limit"; on this codebase it meant the watchdog never armed, and the
ARM port ran for its whole bring-up with **no cross-core wedge detection**, so every hang was silent
instead of a loud panic naming the core and its last task.

> **THE RULE. A stub that returns zero must say, in a comment, whether zero means "disabled" or
> "unlimited" to the neutral caller - and the honest stub for a number a watchdog reads is one that
> disables the feature LOUDLY rather than quietly.** Same class as `panic_halt_check`: a no-op there
> means a panic on one core leaves the others running, which is a machine in an undefined state
> reporting nothing.

### 5. A panic that stops only the panicking core - UNDEFINED STATE, STILL RUNNING

`halt_all_cores` and `panic_halt_check` exist so that a panic on any core stops every core (§6.2,
§19). A port whose `halt_all_cores` only halts the caller leaves the other cores executing against
whatever state the panic was about.

> **THE RULE. If the port cannot signal the other cores yet, SAY SO IN THE STUB.** The loongarch64
> stub does: *"a no-op on this port until its `halt_all_cores` actually signals the other cores"*.
> That is the right shape - an unimplemented thing that names its own consequence.

### What to do with this when you write `arch/<isa>/`

Bring the port up in this order, because each step makes the next one's failures visible rather than
silent:

1. **UART first, and a lock-free writer with it.** Everything below is diagnosed through it, and the
   panic path cannot take a lock.
2. **The trap handler, with the re-entrancy guard from day one.** Not after the first mystery halt.
3. **The watchdog quantum, real.** A wedge you can see is a bug; a wedge you cannot is a week.
4. **`halt_all_cores` that actually reaches the other cores**, before SMP is enabled.
5. **I-cache publication**, before the first service RESPAWN - boot spawns will not show the bug.

Then run `chaos max-carnage` and read the counters, not the summary. Three of the five above were
found by a ratio inside a health line that ended "disk yes".

## Porting a driver: the method (the doctrine)

Supporting new hardware does not mean inventing a driver from the datasheet. Read the **working**
driver - Linux, a BSD, u-boot, or a bare-metal project - and reimplement what the silicon wants as a
GodspeedOS capability service. The condensed rule:

> **The C driver tells us what the silicon wants; we implement that want as a small capability service,
> and we throw away everything about how Linux talks to its own kernel. The hardware knowledge is the
> reusable asset; the OS integration is ours and stays ours.**

What that means in practice:

- **Treat the reference driver as an executable datasheet.** Its register-init sequences, state machines,
  and - most valuable - its *quirks and magic delays* are the reusable part (a real datasheet omits them).
  The Pi 2 DWC2 bring-up cost ~13 hardware iterations rediscovering things `dwc2.c`/u-boot already knew
  (halt all host channels at init; clock the FS PHY at 30/60 MHz not 48); reading the working driver
  would have collapsed that.
- **Reimplement, never translate.** A Linux driver is soaked in `struct device`/URB/workqueue/DMA-API/
  `kmalloc`/threaded-IRQ/sysfs. None of that exists here. Our driver is a **service** (or, until ARM
  routes device IRQs to userspace, a kernel module polled from the tick - see `arch/arm/CLAUDE.md`):
  explicit MMIO/IRQ/DMA-arena caps, IPC, bounded arenas (no heap), **every hardware wait bounded**, loud
  failure + restart. The OS-integration half does not map, so the *understanding* is the only thing that
  crosses - which is also what keeps it clean.
- **Prefer the simplest working reference.** u-boot's dwc2 (polled, ~1k lines) taught more than Linux's
  (interrupt-driven, entangled in usbcore) because it is closer to our model. Use *BSD / u-boot /
  bare-metal for the sequence; use Linux for completeness and quirk-hunting.
- **Scope to the specific chips we run**, not "all hardware" - the RTL8168, the AHCI controller, the Pi
  DWC2 / SD-EMMC / LAN9514. A handful of parts, each one focused driver reading one focused reference.
- **License + provenance.** The kernel is GPL-2.0 (= Linux, compatible); driver *services* link the
  Apache-2.0 SDK, so keep them genuine clean reimplementations - cite the *behaviour* in a comment
  ("Linux `dwc2_init_fs_ls_pclk_sel` selects 30/60 MHz for a HS PHY"), never paste code. A short
  per-driver note recording the extracted sequence and its reference doubles as our own datasheet.

Grokking cuts the *discovery* cost, not the *iteration* cost: QEMU is not silicon, and the OS plumbing
(e.g. routing device IRQs to userspace on ARM) plus real-hardware verification are still our work.

## See also

- `docs/multi-arch.md` - the proof: what compiles, what boots, and the word-size matrix.
- `docs/aarch64.md` - the phased port plan (Phase 0 = seal the boundary; later phases = real port).
- `arch/x86_64/CLAUDE.md` - the full reference surface a mature port exposes.
