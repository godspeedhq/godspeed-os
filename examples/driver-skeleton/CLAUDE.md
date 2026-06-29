# Example: driver-skeleton

How to write a userspace device driver on GodspeedOS. This folder is an annotated **template**: read
`src/main.rs` top to bottom alongside this doc. It compiles (so `cargo build -p driver-skeleton`
checks an adaptation), but it is not runnable as-is - the kernel wires a driver's MMIO/DMA/IRQ per
recognised driver at spawn, so `ctx.mmio()` returns `None` here and it idles. For a real, runnable
driver see `examples/e1000`; for production drivers see `services/block-driver` (AHCI) and
`services/xhci` (USB).

## Purpose

Show that a driver is **just a service** that holds three extra capabilities - an MMIO window, a DMA
arena, and an IRQ line - and that it can drive real hardware while writing **no `unsafe`** and staying
fully restartable. The discipline below is not ceremony: each rule is what keeps a buggy or
compromised driver from taking down the system.

## What it demonstrates

| Phase | The shape | SDK |
|-------|-----------|-----|
| Acquire | get the kernel-granted MMIO window + DMA arena | `ctx.mmio()`, `ctx.dma_region()` |
| Bring up | identity-check, reset, poll for ready, install a DMA ring, enable + unmask the IRQ | `Mmio::read32`/`write32`, `Dma::zero`/`phys_base`, `ctx.irq_unmask(v)` |
| Serve | block for an interrupt or a request, handle it, re-arm | `ctx.recv()`, `Mmio`/`Dma` |
| Degrade | no device mapped, or bring-up fails -> log loudly and idle, never panic | `ctx.try_recv()` + `ctx.yield_cpu()` |

Every register and DMA access goes through the SDK's `Mmio`/`Dma` wrappers. The driver itself
contains no `unsafe`.

## Why it is built this way (the Commandments)

This is the heart of the example. A driver is the one kind of service that touches hardware, so it is
where the discipline matters most.

- **Commandment I + X (a driver is a service, not a kernel change; complexity in the right layer).**
  The kernel does exactly two things for a driver: it routes the device's interrupt to the driver's
  endpoint, and it grants the MMIO/DMA capabilities at spawn. All device logic - the register dance,
  the ring management, the protocol - lives in the driver, in userspace. The unavoidable `unsafe` of a
  volatile register write does not leak into driver code either: it is isolated to the SDK's audited
  `Mmio`/`Dma` layer (§18.1), the one place outside the four kernel layers where `unsafe` is allowed.
  *(COMMANDMENTS.md I, X; CLAUDE.md §4.3, §12, §18.1, §26.10.)*
- **Commandment VII (no ambient authority).** The contract names `hw_mmio = ["0xfeb00000+0x1000"]`
  and `hw_interrupt = [11]`, and the kernel grants caps for **only** that MMIO range and **only** that
  IRQ line. The driver cannot read another device's registers or claim a different interrupt, because
  it never asked for them. On a machine with an IOMMU the guarantee reaches into DMA too: the device
  is confined to the driver's granted arena, so even a compromised driver's DMA engine cannot scribble
  outside it (§6.4 / H1). *(COMMANDMENTS.md VII; CLAUDE.md §12.3, §6.4, Invariant 1.)*
- **Commandment VI (no shared mutable state).** The DMA arena is the driver's *own*, granted to it
  alone; the device reads and writes there by physical address, and results leave the driver over IPC.
  A driver never exposes its hardware through shared memory to another service - that would be
  invisible coupling across an isolation boundary. *(COMMANDMENTS.md VI; Invariant 2.)*
- **Commandment V + IX (your service will restart; plan for recovery).** `bring_up` runs on **every**
  spawn, including a restart, and never assumes the controller kept its state. A driver is restartable
  like any service: on respawn it re-acquires its caps and re-initialises the device from scratch.
  Production drivers prove this - `block-driver`, `xhci`, and `ehci` all re-init and re-enumerate on
  their own restart (`services/CLAUDE.md`). *(COMMANDMENTS.md V, IX; CLAUDE.md §6.2, §14.)*
- **Commandment VIII (wait for truth, not time).** Bring-up resets the device and then **polls the
  STATUS_READY bit** - bounded, so a dead device gives up loudly instead of wedging the core - because
  the bit is the truth; the `yield_cpu()` in the loop only conserves CPU, it never decides readiness.
  The serve loop blocks on the interrupt **event**, never a fixed sleep guessing the device is done.
  *(COMMANDMENTS.md VIII; CLAUDE.md §8.6, §9.3.)*
- **Commandment II (love Chaos; survive Maximum Carnage).** A driver is a prime chaos target - it is
  killed and respawned under load, its frames reclaimed mid-flight. This is not hypothetical: a real
  DMA-after-free, where a dying driver's controller kept DMAing into a freed-and-reused frame, was
  found by `chaos max-carnage` and corrupted a kernel page table. It was fixed at the layer that owns
  it: the kernel quiesces a DMA driver's bus-mastering before reclaiming its frames, and a DMA arena
  is permanently reserved so a stray write can never land in a page table. A driver that respects V,
  VI, VIII, and IX is one that survives this. *(COMMANDMENTS.md II; `milestones/post_v2/4_IOMMU_AND_DMA_SAFETY.md`.)*

## The contract, annotated

```toml
[capabilities]
hw_mmio      = ["0xfeb00000+0x1000"]  # the kernel maps ONLY this range; reach it via ctx.mmio()
hw_interrupt = [11]                    # the kernel routes ONLY this IRQ to our endpoint (§12.2)
log_write    = true
# A DMA arena is granted at spawn to recognised DMA drivers; reach it via ctx.dma_region().
# Recognising a NEW driver is a small kernel-side hook - see examples/e1000.

[placement]
core = 1   # the device's interrupt routes to the core the driver runs on; pinning keeps it deterministic
```

## What you must NOT do

- **Do not write `unsafe` in driver code.** Every hardware access goes through `Mmio`/`Dma`. Reaching
  for a raw pointer breaks §18.2 and **Commandment X** - the unavoidable `unsafe` belongs in the SDK,
  which is audited, not scattered through each driver.
- **Do not busy-sleep waiting for the device.** A `sleep(some_guess)` instead of polling the ready bit
  or waiting on the interrupt breaks **Commandment VIII**; it is both a race and a CPU waste.
- **Do not assume the controller survived a restart.** Re-init on every spawn. Caching device state
  across your own death breaks **Commandment V + IX**.
- **Do not panic when the device is absent.** Degrade and idle (drain the endpoint, yield). A service
  is never special; loud, bounded degradation beats a crash (§3.12).

## How to adapt this

Copy this folder for your device. Replace the illustrative register map with your datasheet's
offsets, fill in `bring_up` (reset, identity, ring/buffer setup) and `handle_request`, and declare
your real `hw_mmio`/`hw_interrupt` (and a DMA arena if the device needs one) in the contract. To make
it actually run, add the kernel-side hook that recognises your driver and maps its BAR - that one
step is shown working in `examples/e1000`.

## See also

- **Commandments I, II, V, VI, VII, VIII, IX, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §12 (drivers and interrupts), §18.1 (the SDK hardware/ABI layer), §6.4 (IOMMU
  confinement).
- `examples/e1000` - a real, runnable driver (reads a live NIC's MAC over MMIO).
- `services/block-driver`, `services/xhci`, `services/ehci` - production drivers.
- `docs/iommu.md`, `milestones/post_v2/4_IOMMU_AND_DMA_SAFETY.md` - the DMA-safety story.
