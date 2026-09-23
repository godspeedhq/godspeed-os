# 39. `hardware_reset` is a spin loop on a non-Pi4 aarch64 build, and prints that it reset the machine

**Status: OPEN, unreachable in any shipping build, fix known.** The aarch64 port has two
`hardware_reset` definitions and the one selected without the `pi4` feature is
`loop { spin_loop() }`. No build we ship selects it, so nothing is broken today; what is recorded is
a trap that is one build flag away and has already cost a hardware debugging session on the sister
port.

## Why this is written down rather than fixed

It is the SAME defect found on riscv64 on 2026-09-21 (`backlog/11`'s neighbour in spirit, though
unrelated in mechanism). There, `reboot` printed `reboot: hardware reset` and then hung the calling
hart forever; ten seconds later the liveness watchdog panicked, correctly, and the operator was
handed a `LIVENESS WEDGE` with no indication that the reset itself was never implemented.

```
reboot: hardware reset
KERNEL PANIC: LIVENESS WEDGE: core 0 made NO progress for 40011360 counter ticks
hart stages at halt: h1=8/60247/s18   (stage 8 = syscall, NR 18 = Reboot)
```

The worst part was not the hang. It was the printed line: **the kernel announced an action it had no
implementation for**, which is invariant 12 broken at the point it is easiest to believe.

## The census, taken 2026-09-21

| arch | `hardware_reset` | shipping? |
|---|---|---|
| `x86_64` | 0xCF9 reset control, then the KBC, then a triple fault | yes |
| `arm` | the BCM2836 watchdog block (`PM_RSTC`/`PM_WDOG`) | yes |
| `aarch64` **with** `pi4` | the BCM2711 watchdog block | yes |
| `aarch64` **without** `pi4` | **`loop { spin_loop() }`** | **no** |
| `riscv64` | SBI SRST, probed, with an honest failure path | yes, fixed 2026-09-21 |
| `riscv32`, `loongarch64`, `s390x` | `loop { spin_loop() }` | no - boot + UART stubs |

The three stub ports are legitimate: they print and halt, run no userspace, and nothing on them can
type `reboot`. The aarch64 arm is different in kind - **that port DOES run userspace**, and the only
thing standing between it and the riscv64 failure is a feature flag that happens to always be set.

## What would close it

`PSCI SYSTEM_RESET` (function id `0x8400_0009`), which is the ARM equivalent of SBI SRST and is what
the riscv64 fix used on its own ISA. It works on any PSCI-capable platform including QEMU's `virt`,
so unlike the Pi-specific watchdog path it is not board knowledge. The aarch64 port already calls
PSCI to start secondary cores, so the mechanism is present and the call site is the only new part.

It is NOT done here for one reason worth stating: it cannot be tested. Every aarch64 build we make
sets `pi4` and therefore takes the other path, so a PSCI reset would be written, audited, and never
executed - which is the exact state `backlog/11` sat in for a fortnight and which this project has
now twice found to be worth less than it looks. When a second aarch64 target exists, this is a small
job with a test attached.

## What is ruled out

- **Not the stub ports.** `riscv32`, `loongarch64` and `s390x` have no userspace and no shell.
- **Not reachable by accident.** `scripts/pi4_build.py` passes `--features pi4,pi4-smp`
  unconditionally; there is no build path that omits it.
- **Not a missing mechanism.** PSCI is already used by this port for `CPU_ON`.
