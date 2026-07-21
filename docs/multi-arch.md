# Multi-Architecture Proof

> **The demarcation, demonstrated.** GodspeedOS is one arch-neutral codebase behind a single seam
> (`arch::imp`). On 2026-07-14 that neutral kernel was compiled and booted on **four genuinely
> different instruction set architectures** - x86-64 (CISC), AArch64 (ARM), RISC-V, and LoongArch - by
> writing only `arch/<isa>/`. Not one arch-neutral file changed between them. On **2026-07-20** a
> fifth joined them, and the first on non-x86 *hardware*: **ARMv7 (32-bit) booting on a Raspberry Pi 2**,
> which also turns the 32-bit word-size claim from a compile result into a running one. This is the
> executable proof of the "a new architecture is bounded to the arch layer" claim (docs/aarch64.md,
> `CLAUDE.md` §26).

## The five booting architectures

| Arch | Rust target | QEMU machine | Boot handoff | Console | Status | Evidence |
|------|-------------|--------------|--------------|---------|--------|----------|
| **x86-64** | `x86_64-unknown-none` | q35 / bare metal (UEFI) | Limine | 16550 COM1 | **Full OS** - 4 cores, supervisor, `gsh>` shell, storage, networking | Hardware (HP T630) + QEMU; identity 24/0; 80k-round chaos soak |
| **AArch64** | `aarch64-unknown-none` | `-M virt -cpu cortex-a53` | direct `-kernel` (EL1) | PL011 @ `0x0900_0000` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-aarch64` |
| **ARM (32-bit)** | `armv7a-none-eabi` | `-M raspi2b` + bare metal | firmware loads `kernel7.img` @ `0x8000` (HYP) | PL011 @ `0x3F20_1000` | **RUNS MULTI-SERVICE IPC ON HARDWARE** - two isolated ring-3 services (`ping`/`pong`) exchange capability-mediated messages through a kernel endpoint under preemptive scheduling (`pong: received "N"`, 6192 clean on the Pi 2, 0 faults); built up from `logger: ready` through timer preemption, per-task address spaces, the banked trap frame, atomic syscalls, and 64 KiB kstacks. Full machine layer + syscalls + user mode + scheduler + IPC | **Raspberry Pi 2 Model B v1.1** (2026-07-21) + QEMU |
| **RISC-V** | `riscv64imac-unknown-none-elf` | `-M virt` | OpenSBI → S-mode @ `0x8020_0000` | NS16550 @ `0x1000_0000` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-riscv64` |
| **LoongArch64** | `loongarch64-unknown-none-softfloat` | `-M virt` | direct `-kernel` (DA mode) @ `0x20_0000` | NS16550 @ `0x1fe0_01e0` | **Boots + prints** to UART; neutral kernel linked | `qemu-system-loongarch64` |
| **s390x** (IBM Z) | `s390x-unknown-none-softfloat` (tier-3, `-Zbuild-std`) | `s390-ccw-virtio` | IPL | SCLP console | **Compiles - BIG-ENDIAN**; boot pending the SCLP console (a protocol, not a register) | `qemu-system-s390x` |

The boot lines actually observed:

```
x86_64 :  kernel: 4 cores ready  /  supervisor: ready  /  shell: ready (type 'help')
aarch64:  GodspeedOS aarch64: _start reached EL1, PL011 UART alive - the demarcation BOOTS.
riscv64:  GodspeedOS riscv64: _start reached S-mode, 16550 UART alive - the demarcation BOOTS on a THIRD arch.
loong64:  GodspeedOS loongarch64: _start reached, 16550 UART alive - the demarcation BOOTS on a FOURTH arch.
arm32  :  GodspeedOS arm32: _start reached SVC, PL011 alive - 32-bit ARM BOOTS.   <-- on REAL HARDWARE (Pi 2)
```

## What is (and isn't) proven

- **Compile-bounded (all five):** `cargo check -p kernel --target <isa>` compiles with **0 errors** for
  AArch64, ARMv7, RISC-V, and LoongArch using only `arch/<isa>/mod.rs`; every neutral file - capability table, IPC,
  scheduler, syscall dispatch, memory, task, loader - typechecks unchanged. Any error *outside*
  `arch/<isa>/` would be a boundary leak; there were none (only stub-completeness gaps in the arch layer
  itself, which the compiler pointed out).
- **Boot-bounded (all five):** each arch's `_start` + minimal boot brings the neutral kernel to life and
  drives its console. x86-64 goes all the way to the interactive shell; AArch64, ARMv7, RISC-V, and LoongArch reach the
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

## Word size: 32-bit as well as 64-bit (proof, recorded for the future)

The 64-bit booting arches and s390x are all 64-bit; arm32 below is the 32-bit one. The next question is whether the neutral kernel is
*word-size* portable - does it silently assume a 64-bit machine? It does not. The only thing that
stood between the codebase and a 32-bit target was **one primitive: `AtomicU64`**. Nothing else - no
pointer-width assumption, no `usize`-is-64 shortcut, no `USER_END` that only fits in 64 bits - blocks
a 32-bit build. `AtomicU64` matters because 32-bit RISC-V (`RV32A`) has only 32-bit atomics, so
`core::sync::atomic::AtomicU64` does not exist there; x86-32 (CMPXCHG8B) and ARMv7 (LDREXD/STREXD)
*do* have 64-bit atomics natively.

The fix is a single dependency - **`portable-atomic`** - which is the native, zero-cost `AtomicU64` on
every ISA that has one (x86/x86-64, ARMv7, all 64-bit arches) and a small lock-based shim only on
32-bit RISC-V. That one line is the whole cost of word-size portability.

| Arch | Rust target | Word | 64-bit atomics? | Status |
|------|-------------|------|-----------------|--------|
| **ARM (32-bit)** | `armv7a-none-eabi` | 32 | Native (LDREXD) | **RUNS MULTI-SERVICE IPC ON HARDWARE** (`ping`->`pong`, 6192 messages, 0 faults) - Raspberry Pi 2 Model B v1.1 (BCM2836, Cortex-A7), 2026-07-21. See below. |
| **RISC-V (32-bit)** | `riscv32imac-unknown-none-elf` | 32 | No (RV32A) → `portable-atomic` shim | **Compiles - 0 errors** (shim proves the shim path) |
| **x86 (32-bit)** | (no upstream `i686-none`) | 32 | Native (CMPXCHG8B) | **Provable, tooling-gated:** the code is word-size-clean (proven by the two above) and has native 64-bit atomics, but rustc ships no bare-metal `i686-none` target; it needs a custom target-spec JSON (a known, small artifact), which hit stable-toolchain friction here. Not a code gap. |

Two 32-bit ISAs compiling with **0 errors** - one native-atomic (ARM), one shim (RISC-V) - covers both
word-size cases end to end: the neutral kernel is 32-bit-clean. x86-32 is the same code with the same
native-atomic story as ARM; only the missing upstream target stands in the way, and that is a
toolchain matter, not a boundary leak. Recorded here so a future 32-bit port starts from "add the
target spec," not "find out whether the kernel is even word-size-portable."

### arm32 RUNS USERSPACE on hardware: `logger: ready` (2026-07-21)

The finish line. A **real GodspeedOS service** - the `logger` crate, compiled for `armv7a-none-eabi`,
loaded from its ELF - runs **unprivileged (PL0)** in its own address space on a **Raspberry Pi 2 Model
B v1.1** and logs through a **capability-checked `svc`** into the neutral kernel:

```
arm32: ===> below this line is a real GodspeedOS SERVICE running unprivileged <===
logger: ready
```

That one line is the whole userspace stack meeting on 32-bit ARM: the neutral frame allocator hands
out the service's frames, the SVC entry serves its syscall, PL0 + USER page permissions run it
unprivileged, the SDK+service built for ARM is the code, the neutral ELF loader mapped it, and a
minimal spawn wired a task with a `LOG_WRITE` capability. `ctx.log("logger: ready")` issues `svc #0`;
the neutral `handle_log` validates the cap and prints. Nothing in the syscall/IPC/capability/scheduler
core is ARM-specific - only `arch/arm/` was written.

**The hardest bug, and the one only hardware truly settles.** The service reached PL0 and issued its
Log syscall, but the kernel hung acquiring the cap-table spinlock (`GLOBAL_RESOURCES`) under the
service's address space - while `kprintln`'s lock worked. The kernel maps its memory as 1 MiB
**sections** but the service maps the shared 1 MiB (holding the service-context page) as 4 KiB
**pages**; stale D-cache lines from the section view made the lock's `LDREX`/`STREX` fail under the
page view (a coherency, not attribute, mismatch). A set/way `DCCISW` D-cache clean before the TTBR0
switch - correct across any address-space change - fixes it, and it held on hardware first try.
Diagnosis used the full toolkit: `-d int` (service runs past the svc), symbol addresses
(`GLOBAL_RESOURCES` at `0x3f0078`), `ATS1CPR` (the page IS mapped), and lock-vs-lock_irq isolation.

Gated behind the `arm-spawn-logger` build feature (the default image boots to the clean selftest
halt). This is a **minimal** spawn - one service, one capability, entered directly rather than through
the full scheduler; IPC endpoints, the registry, the supervisor manifest, and running the *neutral*
`scheduler::run` are the remaining work toward a full multi-service boot.

Everything below is the machine layer under it, each part verified on the silicon by its own selftest:

### arm32 runs a COMPLETE kernel machine layer on hardware (2026-07-21)

The neutral kernel does far more than print on a **Raspberry Pi 2 Model B v1.1** (BCM2836, Cortex-A7,
ARMv7) now. A full machine layer is up and **every layer is verified on the silicon** by its own
selftest:

```
arm32: exception vectors installed (VBAR = 0x000080a0)
arm32: DTB memory node - base 0x00000000, size 0x3b400000 (948 MiB), end 0x3b400000
arm32: MMU ON (short descriptors, 1 MiB sections, L1 @ 0x00020000)
arm32: MMU selftest PASS (RAM + MMIO identity, unmapped faults)
arm32: caches ON (I + D + branch prediction)
arm32: generic timer CNTFRQ = 19200000 Hz  ... measured 1000000 Hz  (BCM2836 quirk, using MEASURED)
arm32: tick selftest PASS (timer IRQ fires at the requested rate)
arm32: context selftest PASS (two kernel contexts switch and resume)
arm32: preempt selftest PASS (timer rotates between tasks that never yield)
arm32: neutral surface PASS (TaskContext::new_kernel + switch_context drive an ARM task)
arm32: pgtable PASS (4 KiB map translates; read-only is enforced)
```

What that covers, all `arch/arm/` only, all hardware-proven: boot (HYP->SVC drop, core parking,
VFP/NEON), PL011 console, **exception vectors** with fault decoding, the **MMU** (1 MiB sections +
two-level **4 KiB pages** with enforced read-only permissions) and caches, the **generic timer**, a
100 Hz **timer-interrupt tick** through the BCM2836 core-local controller, **cooperative and
preemptive context switching**, the flattened **device-tree** memory map, and the **neutral
context-switch surface** the scheduler imports. Everything the neutral kernel needs *below userspace*
is real on 32-bit ARM.

**Four bugs that only hardware could find.** Each passed cleanly under QEMU and failed on silicon -
the argument for hardware-in-the-loop bring-up, recorded so the next porter expects them:
- **`CNTFRQ` lies by 19.2x.** The BCM2836 divides the 19.2 MHz crystal to 1 MHz via a core-timer
  prescaler and never updates `CNTFRQ`. Caught only because the timer was cross-checked against the
  independent 1 MHz System Timer; the tick is programmed from the *measured* rate.
- **The timer IRQ source is secure-state-dependent.** `CNTP_*` addresses the secure or non-secure
  physical timer by CPU state - different interrupt bits (`CNTPSIRQ` vs `CNTPNSIRQ`). Hardware boots
  non-secure (HYP), QEMU's stub boots secure; routing one worked on neither. Route both.
- **`install()` reset the CPU** by holding a value in `r12` across an FIQ-mode switch, which banks
  r8-r12. Symptom: a double boot banner and `VBAR = 0`.
- **Page-table writes stranded in the D-cache.** Walks are non-cacheable, so the walker reads the PoC;
  cacheable descriptor stores must be cleaned out with `DCCMVAC` or the first translation faults. The
  SEC-28 DMA-coherence class, applied to the table walker.

Still pending: **userspace** - `memory::init` from the DTB (retiring the page-table static arena for
`alloc_frame`), user mode (PL0) via a fabricated SPSR return, the SVC syscall ABI, and building ARMv7
service ELFs. That is the "entire port" `docs/aarch64.md` scopes; the machine layer below it is done.

**ARMv7 is a SEPARATE PORT from AArch64, not a variant of it.** `arch/arm/` and `arch/aarch64/` share
zero code: processor modes and CP15 (`MRC`/`MCR`) instead of exception levels and system registers
(`MRS`/`MSR`), different MMU descriptors, different vector tables, `LDREXD` instead of `LDXR`/`STXR`.
Budget a 32-bit ARM port as its own work; almost nothing carries over from the 64-bit one.

**ARMv7 is a SEPARATE PORT from AArch64, not a variant of it.** `arch/arm/` and `arch/aarch64/` share
zero code: processor modes and CP15 (`MRC`/`MCR`) instead of exception levels and system registers
(`MRS`/`MSR`), different MMU descriptors, different vector tables, `LDREXD` instead of `LDXR`/`STXR`.
Budget a 32-bit ARM port as its own work; almost nothing carries over from the 64-bit one.

**Bring-up notes (each one cost something to find):**

- **The firmware enters in HYP mode.** Cortex-A7 has the virtualization extensions, and the Pi
  firmware enters an ARMv7 kernel in HYP (mode `0x1A`) whenever a device tree is loaded - which the
  stock Raspberry Pi OS `config.txt` does. `_start` checks CPSR and `eret`s down to SVC only if it is
  actually in HYP, so one image works either way. This is the ARMv7 counterpart of the AArch64
  `CPACR_EL1.FPEN` trap. **QEMU's `raspi2b` stub hands over in SVC, so emulation never exercises this
  branch** - it went straight from unproven to working on hardware, which is exactly the kind of gap
  emulation hides.
- **`.arch_extension virt` is mandatory.** `armv7a-none-eabi` does not enable the virtualization
  extensions, so the assembler rejects `spsr_hyp`/`elr_hyp`/`eret`. The silicon has them; only the
  default target description is conservative.
- **Do not assume firmware initialised the UART.** On hardware it has (Linux runs a console at
  115200), but `qemu-system-arm -kernel` has no firmware: the PL011 comes up disabled, every write to
  DR is swallowed, and `FR.TXFF` reads 0 so the transmit poll never even blocks. Output simply
  vanishes. `pl011_init` sets `LCRH`/`CR` explicitly but **leaves IBRD/FBRD alone** - the reference
  clock differs between firmware and emulation, so recomputing divisors would break baud on one of the
  two targets.
- **Load address: 0x8000, and QEMU disagrees.** The Pi firmware loads `kernel7.img` flat at `0x8000`
  (the `7` selects the ARMv7 image *by name*), so `_start` must be physically first - `.text.boot` +
  `KEEP` + `ENTRY`. QEMU's generic AArch32 loader instead uses `0x10000`, and the mismatch is vicious:
  the literal-pool `__bss_start` resolves into the *running code*, so the BSS-zero loop **overwrites
  its own instructions** (visible in `-d in_asm` as live opcodes turning into `00000000`). Test under
  QEMU with `-device loader,file=kernel7.img,addr=0x8000 -device loader,addr=0x8000,cpu-num=0`.
- **All four cores start.** Read `MPIDR` and park cores 1-3 in `WFE`. Later SMP work takes them off
  the firmware mailboxes at `0x4000_008C + 0x10*core` instead (the Pi 2 has no PSCI and no GIC - that
  is Pi 4 hardware).
- **Deploying is a file copy, not a flash.** `kernel7.img` is a raw ARM binary that goes *on* the
  existing FAT32 boot partition beside `bootcode.bin`/`start.elf` - it is not a disk image, and Rufus
  correctly refuses it. (Windows may leave that partition without a drive letter; assign one via
  `Set-Partition -DiskNumber N -PartitionNumber 1 -NewDriveLetter E` from an elevated shell.)

## The point

The value was never "GodspeedOS runs on ARM." It's that a capability microkernel kept *small enough to
audit exhaustively* has an arch boundary *clean enough that a second, third, and fourth ISA are bounded drops-in* -
proven by the compiler and by four QEMU consoles, not by argument. The intense commandment audits were
the groundwork; this is the payoff.
