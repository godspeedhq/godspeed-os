# kernel/src/arch/

The architecture layer: the unsafe hardware boundary (§18.1), and **the single seam a new ISA drops
into**. Everything CPU-specific lives under `arch/<isa>/`; the rest of the kernel is arch-neutral and
reaches the hardware only through this directory.

If you are porting GodspeedOS to a new architecture, this file is your map. The claim it backs is the
one proven in `docs/multi-arch.md`: **a new architecture is bounded to `arch/<isa>/` - you write that
directory and nothing else in the kernel changes.** Five ISA families (x86-64, AArch64, RISC-V,
LoongArch, s390x) and both word sizes (64-bit and 32-bit) have been proven this way.

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

For the **first milestone** of a new arch - boot the neutral kernel and print to a UART - the surface
is far smaller: a `_start`, minimal CPU/stack setup, and a byte-out to the platform's serial device.
The existing non-x86 stubs (`arch/aarch64/mod.rs`, `arch/riscv64/mod.rs`, `arch/loongarch64/mod.rs`,
`arch/riscv32/mod.rs`, `arch/arm/mod.rs`) are exactly that milestone and are the templates to copy.
The full port (MMU, exception vectors, syscalls, interrupt controller, timer, SMP, the userspace
SDK/services) is deliberate later work, tracked in `docs/aarch64.md`.

## Adding an architecture: the checklist

Everything you touch is in one of five places. None of them is a neutral kernel file.

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

5. **`scripts/arch_boundary_check.py`** - extend the `_ARCHES` regex with your arch name so the guard
   *also* forbids neutral code from naming your arch directly. A boundary that does not know about
   your arch cannot protect it.

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
(`docs/security-audit.md`) - **port blockers**, gathered here so a porter meets them by construction
instead of rediscovering them as heisenbugs. (They do not affect x86, so they are not "fixed" in code on
`feat/hardening`; they are specified here for whoever brings up SMP on a weak arch.)

**1. Task-slot publication ordering (SEC-25).** The scheduler publishes a slot with a flag store and
reads it with a flag load, then touches plain data fields (`TASK_CTX`, `TASK_IS_USER`,
`TASK_KERNEL_STACK_TOP`, ...). For the data to be visible whenever the flag is, the *writer* stores the
data **before** the flag with **Release**, and every *reader* loads the flag with **Acquire** before
touching the data. Two concrete port fixes:
- `reserve_task_slot` currently stores `TASK_VALID[i] = true` (Release) *before* `TASK_CORE[i]` - reorder
  so `TASK_CORE` (the data) is written first and `TASK_VALID` is the Release that publishes it.
- The ~30 `TASK_VALID.load(Relaxed)` reader sites (and field reads gated on them) become **Acquire**. On
  x86 an Acquire load is a plain `mov` (identical codegen); on AArch64/RISC-V it emits the barrier that
  establishes happens-before. `commit_task` already publishes fields then `TASK_STATE = Ready` (Release);
  the SEC-1 switch-in path is already `SeqCst`.

  Without this, a weak-arch reader can observe `VALID`/`Ready == true` with a **stale `TASK_CTX`/CR3/
  kstack** - the same use-after-free class as SEC-1.

**2. An address-space switch must flush the TLB (SEC-26 / SEC-27).** The neutral kill path *elides* the
cross-core TLB shootdown for a pinned task ("a CR3 reload flushes non-global TLB entries"). That is an
**x86 semantic**. On AArch64 a `TTBR0_EL1`+ASID switch does not implicitly flush; RISC-V `satp` needs an
explicit `sfence.vma`. So the `arch::imp` context-switch / `write_page_table_base` primitive on a weak
arch MUST either (a) flush the outgoing address space's non-global entries on the switch, or (b) the
neutral kill path must issue the cross-core shootdown it currently elides. `invalidate_tlb_page` is
local-core on x86 but broadcasts on ARM (`TLBI VAE1`) - a correctness-neutral but worth-knowing
difference.

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

## See also

- `docs/multi-arch.md` - the proof: what compiles, what boots, and the word-size matrix.
- `docs/aarch64.md` - the phased port plan (Phase 0 = seal the boundary; later phases = real port).
- `arch/x86_64/CLAUDE.md` - the full reference surface a mature port exposes.
