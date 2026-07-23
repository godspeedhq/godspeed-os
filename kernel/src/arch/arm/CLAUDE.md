<!-- SPDX-License-Identifier: GPL-2.0-only -->
# kernel/src/arch/arm/ (ARMv7-A, 32-bit - Raspberry Pi 2 / BCM2836)

The 32-bit ARM implementation of the `arch::imp` surface. This is a **separate port from AArch64**
(`arch/aarch64/`) - processor modes + CP15 (`MRC`/`MCR`), not exception levels + system registers;
short-descriptor MMU; `LDREXD` not `LDXR`. They share **zero** code. See `docs/multi-arch.md` for the
cross-arch picture and `docs/arm32-status.md` for what runs today + how to build/run it.

> **First stop for anything ARM32:** `docs/arm32-status.md` (state, build/run commands, known issues,
> remaining work). This file is the *implementer's* reference: ABI, boot flow, the driver rules, and the
> port hazards.

## The syscall ABI (what a service/SDK author must respect)

Userspace issues **`svc #0`**. The register convention (matched by the SDK's `raw_syscall`,
`sdk/rust/src/syscall.rs`, and the kernel's `arm_svc_dispatch`, `arch/arm/syscall.rs`):

| Register | Role |
|----------|------|
| `r0`     | syscall number, **then** the low 32 bits of the `i64` result |
| `r1`     | arg0, **then** the high 32 bits of the `i64` result |
| `r2`     | arg1 |
| `r3`     | arg2 |

- **Each argument is ONE 32-bit register.** The neutral `syscall_handler` takes `u64`s; `arm_svc_dispatch`
  takes `u32`s (one register each) and widens. Every current syscall arg - pointer, handle, cap slot,
  length - genuinely fits in 32 bits on this arch, so the widening is loss-free **for those**.
- **The one exception: a value that can exceed 32 bits.** A `recv_timeout` in generic-timer ticks can
  (u32::MAX ticks is ~68 s at the Pi 2's ~62.5 MHz CNTFRQ). Such an arg is **truncated** by the single-
  register ABI, so its SDK wrapper must **pre-clamp it on ARM** before the syscall (`recv_timeout`
  saturates to `[1, u32::MAX]`; a genuine 0 = block-forever is preserved). **Any new syscall passing a
  wider-than-u32 value on ARM must do the same** (clamp in the wrapper, or split into a register pair).
  This is userspace-audit A-U1 - the class of bug to watch for on a 32-bit ABI.
- The `i64` result returns in `r0:r1` (low:high), sign-extended, so negative error codes reconstruct
  correctly.

## Boot flow

`_start` (Pi firmware enters in HYP when a device tree is loaded) `eret`s down to SVC, sets up a stack,
and calls `arm_boot_main` (`mod.rs`). That runs the machine bring-up (MMU short-descriptor tables,
exception vectors at `VBAR`, generic timer, PL011, frame allocator, DWC2 probe) + boot selftests, then
dispatches to **one** boot path selected by a cargo feature: `arm-supervisor` (the real OS: kernel spawns
the supervisor, which spawns logger + shell + ping/pong) or `arm-shell` (kernel spawns logger + shell
directly). The shipping build is `arm-supervisor` with the supervisor's `bare-metal` feature (clean
`gsh>` prompt). `docs/arm32-status.md` has the build/run commands.

> **The cr3/TTBR0-seed rule (every boot path):** mask IRQs (`irq::disable_interrupts()`) **before**
> `NEUTRAL_SCHED.store(true)`. A timer that preempts into the scheduler context before `scheduler::run(0)`
> seeds its TTBR0 leaves that context with TTBR0=0, and the first task to block wedges the core. All
> `sched_*.rs` paths do this (kernel-audit Audit 5).

## Drivers on ARM: userspace is the rule, in-kernel is the current exception

The constitution (§12, §4.4) says drivers are **userspace services** reached through interrupt routing.
The ARM port does **not yet route device IRQs to userspace**, so a driver here currently runs
**in-kernel** and polls its device from the timer tick (the PL011-console model), pushing results into
the same input ring the shell reads. `arch/arm/dwc2.rs` (the USB host driver) is the worked example.
This is a temporary port limitation, not a new policy: when ARM routes device IRQs to userspace, drivers
move out to services like x86's. Until then a new Pi driver (SD/EMMC, etc.) follows the `dwc2.rs`
pattern: `arch/arm/` module, safe MMIO via `read_volatile`/`write_volatile` on the Device-mapped
peripheral window, **every hardware wait bounded** (a dead/absent device must never hang the boot -
invariant 12; `dwc2.rs`/`video.rs`/`timer.rs` are the models after kernel-audit Audit 5).

**How to turn a datasheet into a GodspeedOS driver:** see the ratified method in `arch/CLAUDE.md`
("Porting a driver: the method") - grok a *working* reference (u-boot / Linux / bare-metal), reimplement
what the silicon wants, throw away the OS integration.

## The ARM SMP / weak-memory obligations (a Cortex-A7 is weak-ordered)

`arch/CLAUDE.md` (SEC-25..28) is the contract. Status on this port:

- **SEC-25 (task-slot publication ordering): DONE** - the scheduler writes data before the `TASK_VALID`
  Release flag and reads it Acquire.
- **SEC-26/27 (TLB flush on address-space switch): DONE** - `switch_context` writes TTBR0 then
  `TLBIALL`+`dsb`+`isb`. Note `invalidate_tlb_page` is **local** (`TLBIMVA`), correct for pinned per-task
  address spaces; a future cross-core unmap would need the inner-shareable variant.
- **SEC-28 (DMA cache coherence): a live blocker for any DMA driver.** The A7's DMA is **not** cache-
  coherent. A driver that DMAs must either map its arena **non-cacheable** or bracket every transfer with
  cache maintenance (clean-to-PoC before a device read of a CPU-written buffer, invalidate before a CPU
  read of a device-written buffer). The SDK's `Dma` wrapper (`sdk/rust/src/dma.rs`) assumes coherent x86
  DMA and does none of this, so it is **not yet reusable on ARM** as-is. (`dwc2.rs` sidesteps it by using
  PIO, not DMA.)

## Gotchas (found by booting - see `docs/arm32-status.md` for the full list)

- **Build the usable OS in `--release`.** Debug (unoptimized) shell pipe frames (~600 KiB) exceed the
  256 KiB user stack and fault the shell (it recovers via supervisor restart); release frames fit.
- **No RTC on the Pi 2** (QEMU raspi2b emulates none) - `date`/`uptime` read zeros. Not a bug.
- **Serial console: 115200 8N1** on the Pi's PL011 (GPIO14/15), same as x86.
- **DWC2 register lessons** (halt-all-channels at init, `FSLSPClkSel=0` for the HS PHY, HPRT write-1-to-
  disable trap) are in the `dwc2.rs` comments + git log - the kind of hard-won quirk the doctrine says to
  mine from a working reference.

## What was audited here

- **Kernel:** `docs/kernel-audit.md` **Audit 5** (2026-07-23) - the arm32 layer; 8 (C) fixed, 2 staged.
- **Userspace/SDK:** `docs/userspace-audit.md` **Audit 4** - the arm SDK ABI (A-U1 above).
- **Unsafe inventory:** `docs/unsafe-audit.md` lists every `unsafe` block in `arch/arm/`.
