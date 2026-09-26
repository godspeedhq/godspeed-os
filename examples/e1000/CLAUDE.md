# Example: e1000

A real, runnable userspace driver for the Intel 82540EM ("e1000") NIC. This is the runnable
counterpart to `examples/driver-skeleton`: where the skeleton is an annotated template, this driver
actually boots under QEMU, reads a live NIC over MMIO, and logs what it finds. It is deliberately
small and **read-only** - it reports the link state and the MAC the NIC loaded from its EEPROM - so
the whole thing fits in one screen and the discipline stays visible.

## Purpose

Prove that a contributor can drive **new** hardware on GodspeedOS end to end: declare a service, name
its device CLASS in the supervisor's spawn row, read device registers through the safe SDK wrapper, and
run - all without writing `unsafe`, without expanding the kernel's responsibilities, and **without a
kernel source change of any kind**.

## What it demonstrates

| Step | The shape | SDK |
|------|-----------|-----|
| Acquire | get the kernel-granted MMIO window for the NIC | `ctx.mmio()` |
| Read | Device Status (link up?), Receive Address (the MAC) | `Mmio::read32` |
| Report | log link + MAC | `ctx.log` / `ctx.log_fmt` |
| Degrade | no e1000 mapped -> log and idle, never crash | `gs::ipc::try_recv` + `gs::task::yield_now` |

Registers used (byte offsets into BAR0): `STATUS 0x0008` (bit 1 = Link Up), `RAL0 0x5400` + `RAH0
0x5404` (the 6-byte MAC the NIC auto-loaded from its EEPROM). Reads go through the SDK `Mmio` wrapper,
so the driver contains **no `unsafe`**.

## Why it is built this way (the Commandments)

- **Commandment I + X (a driver is a service, and needs NO kernel change at all).** This example
  needs none: a driver names its device CLASS in the SPAWN REQUEST the supervisor sends, and the
  kernel resolves that against its own bus scan. `README.md` says the same - "It needs no kernel
  source change."

  > **Corrected 2026-09-26.** This said the example needs "one entry in the `service_hw` table in
  > `kernel/src/task/mod.rs` (`"nic-driver" | "e1000" => HwClass::Nic`)". That table has NO ARMS
  > left - only `_ => (HwClass::None, false)`, under a comment reading "EVERY driver has moved to
  > the supervisor (step C) ... Nothing is left to look up by name here." Following the old
  > instruction meant editing ring 0 to add a name to a table that no longer reads names. The
  > vendor whitelist this document also described (`0x100E_8086`) went the same way: step D
  > removed it, so the kernel no longer needs to have been taught your device.

  The kernel still does its one job - granting a hardware capability - and nothing more. All device
  logic lives here, in userspace, and the volatile-register `unsafe` stays isolated in the SDK
  `Mmio` layer (§18.1). *(COMMANDMENTS.md I, X;
  CLAUDE.md §4.3, §12, §18.1, §26.10.)*
- **Commandment VII (no ambient authority, made concrete).** The driver reaches the NIC's registers
  only because its spawn row asked for that device CLASS and the kernel's own bus scan found one. It
  is granted a window, never given the bus. Where no matching device is found the grant never
  happens, so `ctx.mmio()` returns `None` and the driver touches no foreign hardware.

  > **Corrected 2026-09-26.** This said the kernel maps the BAR `if name == "e1000" && the
  > discovered NIC is actually an Intel e1000 (vendor/device 0x100E8086)`. That vendor whitelist is
  > gone - step D removed it precisely so the kernel need not have been taught your device - and the
  > by-name gate went with `service_hw`'s arms. The no-ambient-authority property is unchanged and
  > now rests on the class the spawn request names. With an
  IOMMU present a full driver's DMA would be confined to its arena too (§6.4 / H1).
  *(COMMANDMENTS.md VII; CLAUDE.md §12.3, §6.4, Invariant 1.)*
- **Commandment V (no service is special).** When no e1000 is mapped - it is absent, or the machine
  has a different NIC like the T630's chipset - the driver logs the situation and idles. It never
  panics, never assumes the device is there. Degradation is loud and bounded. *(COMMANDMENTS.md V;
  CLAUDE.md §3.12, §6.2.)*
- **Commandment VI (no shared mutable state).** This read-only example only reads registers, but the
  pattern it points to holds: a full driver's TX/RX rings live in its *own* DMA arena, and packets
  cross to a network stack over IPC - never a shared-memory window into the driver. *(COMMANDMENTS.md
  VI; Invariant 2.)*
- **Commandment II (survive Maximum Carnage).** A driver is a prime chaos target: killed and
  respawned under load with its frames reclaimed mid-flight. A real DMA-after-free of exactly this
  shape - a dying driver's controller still DMAing into a freed-and-reused frame - was found by `chaos
  max-carnage` and corrupted a kernel page table. It was fixed at the layer that owns it: the kernel
  quiesces a DMA driver's bus-mastering before reclaiming its frames, and each driver's DMA arena is
  permanently reserved so a stray write can never reach a page table. A NIC driver you grow from this
  must survive that. *(COMMANDMENTS.md II; `milestones/hardware/iommu-and-dma.md`.)*

## The contract, annotated

```toml
[capabilities]
log_write = true
# The NIC's MMIO BAR is granted at spawn because the supervisor's spawn row for this service names a
# device CLASS, which the kernel resolves against its own bus scan (the same route xhci, ehci and
# block-driver take). NOT by name, and NOT from this file: nothing parses TOML at spawn (13.6). Reach
# the window via ctx.mmio(). A read-only driver needs no DMA arena and no hw_interrupt; a full NIC
# driver would add both (see examples/driver-skeleton for that shape).

[placement]
core = 1
```

## How to run it

Boot the OS under QEMU with an e1000 NIC attached (`-device e1000,netdev=n0 -netdev user,id=n0` - the
shell test already boots this way) and the supervisor spawning `e1000`. On the wire you will see:

```
e1000: link UP  MAC 52:54:00:12:34:56
```

(`52:54:00:12:34:56` is QEMU's default e1000 MAC.) On the T630, whose NIC is not an Intel e1000, the
BAR is never mapped, so instead you see the honest idle line:

```
e1000: no Intel e1000 mapped (absent, or a different NIC) - idling
```

## What it would take to make it a full driver

Read-only "what NIC is this" is the first rung. A real driver would, in order: declare a DMA arena
and `hw_interrupt` in its contract; build TX and RX descriptor rings in the arena and hand the device
their physical addresses; enable the device and unmask its IRQ (`ctx.irq_unmask`); on each interrupt,
walk the rings for completed packets and hand them to a network stack over IPC; and re-initialise all
of it on every restart (Commandments V + IX). `docs/networking.md` sketches that NIC driver and the
"a socket is a capability" model it feeds.

## What you must NOT do

- **Do not write `unsafe` to poke the registers.** Use `ctx.mmio()` + `Mmio::read32`/`write32`. Raw
  pointers break §18.2 and **Commandment X**.
- **Do not assume the NIC is an e1000.** The kernel gate already enforces this; mirror it in spirit -
  degrade when `ctx.mmio()` is `None` rather than reading garbage and trusting it (**Commandment V**).
- **Do not widen the device class into "any NIC's BAR for anyone".** Ask for the specific class your
  driver was written for; a broad grant is ambient authority (**Commandment VII**).
- **Do not panic on a down link or a zero MAC.** Report it and carry on; loud, bounded behaviour over
  a crash (§3.12).

## How to adapt this

To drive a different PCI device: give it a row in the supervisor's spawn table naming its device
class - or its PCI class CODE, which is how AHCI is addressed - write the service against
`ctx.mmio()` (and `ctx.dma_region()` / `ctx.irq_unmask()` if it needs DMA or interrupts), and add it
to the workspace. **No kernel edit.** If your device needs a `HwClass` variant that does not exist
yet, that is the one case that touches `kernel/`, and it is a new device CLASS rather than a new
name.
`examples/driver-skeleton` is the full template for the device-bringup shape.

## See also

- `examples/driver-skeleton` - the annotated driver pattern (reset, ring, interrupt, restart).
- `services/block-driver` (AHCI), `services/xhci`, `services/ehci` - production drivers.
- `docs/networking.md` - the future NIC driver and socket-as-capability.
- **Commandments I, II, V, VI, VII, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §12 (drivers and interrupts), §18.1 (the SDK hardware/ABI layer), §6.4 (IOMMU
  confinement); `milestones/hardware/iommu-and-dma.md` (the DMA-safety story).
