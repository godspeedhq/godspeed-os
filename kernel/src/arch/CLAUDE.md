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

## See also

- `docs/multi-arch.md` - the proof: what compiles, what boots, and the word-size matrix.
- `docs/aarch64.md` - the phased port plan (Phase 0 = seal the boundary; later phases = real port).
- `arch/x86_64/CLAUDE.md` - the full reference surface a mature port exposes.
