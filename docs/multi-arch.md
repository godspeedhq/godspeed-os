# Multi-Architecture Proof

> **The demarcation, demonstrated.** GodspeedOS is one arch-neutral codebase behind a single seam
> (`arch::imp`). On 2026-07-14 that neutral kernel was compiled and booted on **four genuinely
> different instruction set architectures** - x86-64 (CISC), AArch64 (ARM), RISC-V, and LoongArch - by writing
> only `arch/<isa>/`. Not one arch-neutral file changed between them. This is the executable proof of
> the "a new architecture is bounded to the arch layer" claim (docs/aarch64.md, `CLAUDE.md` §26).

## The four architectures

| Arch | Rust target | QEMU machine | Boot handoff | Console | Status | Evidence |
|------|-------------|--------------|--------------|---------|--------|----------|
| **x86-64** | `x86_64-unknown-none` | q35 / bare metal (UEFI) | Limine | 16550 COM1 | **Full OS** - 4 cores, supervisor, `gsh>` shell, storage, networking | Hardware (HP T630) + QEMU; identity 24/0; 80k-round chaos soak |
| **AArch64** | `aarch64-unknown-none` | `-M virt -cpu cortex-a53` | direct `-kernel` (EL1) | PL011 @ `0x0900_0000` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-aarch64` |
| **RISC-V** | `riscv64imac-unknown-none-elf` | `-M virt` | OpenSBI → S-mode @ `0x8020_0000` | NS16550 @ `0x1000_0000` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-riscv64` |
| **LoongArch64** | `loongarch64-unknown-none-softfloat` | `-M virt` | direct `-kernel` (DA mode) @ `0x20_0000` | NS16550 @ `0x1fe0_01e0` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-loongarch64` |

The boot lines actually observed:

```
x86_64 :  kernel: 4 cores ready  /  supervisor: ready  /  shell: ready (type 'help')
aarch64:  GodspeedOS aarch64: _start reached EL1, PL011 UART alive - the demarcation BOOTS.
riscv64:  GodspeedOS riscv64: _start reached S-mode, 16550 UART alive - the demarcation BOOTS on a THIRD arch.
loong64:  GodspeedOS loongarch64: _start reached, 16550 UART alive - the demarcation BOOTS on a FOURTH arch.
```

## What is (and isn't) proven

- **Compile-bounded (all four):** `cargo check -p kernel --target <isa>` compiles with **0 errors** for
  AArch64, RISC-V, and LoongArch using only `arch/<isa>/mod.rs`; every neutral file - capability table, IPC,
  scheduler, syscall dispatch, memory, task, loader - typechecks unchanged. Any error *outside*
  `arch/<isa>/` would be a boundary leak; there were none (only stub-completeness gaps in the arch layer
  itself, which the compiler pointed out).
- **Boot-bounded (all four):** each arch's `_start` + minimal boot brings the neutral kernel to life and
  drives its console. x86-64 goes all the way to the interactive shell; AArch64, RISC-V, and LoongArch reach the
  UART-print milestone (the full port - MMU, exception vectors, syscalls, GIC/PLIC, timer, SMP, and the
  userspace SDK/services - is deliberate future work, tracked in docs/aarch64.md).
- **Enforced, not just achieved:** the boundary is held by four CI guards (`unsafe_check`,
  `contract_check`, `arch_boundary_check`, `dash_check`) plus the multi-arch compile itself. A future ISA
  is a drop-in: add `arch/<new>/` to the `imp` surface, add the `#[cfg(target_arch)]` arm, and CI proves
  no neutral file smuggled in arch-specific code.

## Per-arch bring-up notes (for the next porter)

- **Boot handoff:** x86 via Limine (higher-half, PHDRS); AArch64 via QEMU `-kernel` at `0x4008_0000` in
  EL1; RISC-V via OpenSBI (M-mode firmware) into S-mode at `0x8020_0000`; LoongArch via direct `-kernel` in DA mode (VA==PA) at `0x20_0000`.
- **First-instruction gotchas found by booting:** AArch64 traps FP/SIMD at EL1 by default - Rust emits
  NEON for `memcpy`/byte-copy, so `_start` must enable `CPACR_EL1.FPEN` before any Rust (found via
  `qemu -d int` → ESR `0x07`); AArch64 SP must be 16-byte aligned (EL1 SP-align check). RISC-V used
  `riscv64imac` (soft-float), sidestepping the FP-enable step entirely, and booted first try - as did LoongArch (`-softfloat`).
- **Linker scripts:** `kernel/kernel.ld` (x86, higher-half), `kernel-aarch64.ld` (virt `0x4008_0000`),
  `kernel-riscv64.ld` (virt `0x8020_0000`), `kernel-loongarch64.ld` (virt `0x20_0000`). `kernel/build.rs` selects by target and embeds an empty
  service-ELF placeholder for the non-x86 targets (real cross-arch services are future work).

## The point

The value was never "GodspeedOS runs on ARM." It's that a capability microkernel kept *small enough to
audit exhaustively* has an arch boundary *clean enough that a second, third, and fourth ISA are bounded drops-in* -
proven by the compiler and by four QEMU consoles, not by argument. The intense commandment audits were
the groundwork; this is the payoff.
