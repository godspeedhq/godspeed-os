# Taking the DWC2 USB stack out of the arm32 kernel

> **Status:** in progress on `feat/pi2-arm32-hardening`. Phase 1 landed (`58eb4610`).
>
> **Goal:** `kernel/src/arch/arm/dwc2.rs` is **3981 of the arm32 arch layer's 10732 lines - 37% of it
> is a USB driver**, running in ring 0, parsing descriptors supplied by whatever a user plugs in.
> Move it to a userspace service, as the AArch64 port did with `xhci`.

## Why the stated blocker was not one

`CLAUDE.md` §6.4 (SEC-29/SEC-30) justifies the in-kernel stack on the grounds that *"ARM does not yet
route device IRQs to userspace"*, and treats that as a property of the hardware. It is not. The
interrupt was already being received, confirmed and dispatched in `arm_irq_dispatch` - it went to the
kernel's own driver because nothing else had ever asked for it. The neutral router
(`kernel/src/interrupt/route.rs`) is arch-agnostic and complete; nobody had connected the two.

**This is the second time that mistake has been found.** AArch64 made the identical claim about the
GIC; the comment left at that fix reads *"an unimplemented branch was read as an architectural
constraint, and a Commandment I violation was accepted on the strength of it."* Twice is a pattern.
A port should check whether the branch exists before conceding that the hardware forbids it.

## Phases

| Phase | Work | State |
|---|---|---|
| **1** | Route the USB IRQ to userspace *when a service has registered for it*; real mask/unmask on the BCM2835 legacy controller | ✅ `58eb4610` - hardware-verified inert (selfcheck 350/0 + 351/0, chaos 50 rounds, no panics) |
| **2** | A skeleton `dwc2` service holding MMIO + DMA + `hw_irqs = [0x29]`, which does nothing but prove the interrupt ARRIVES in userspace | in progress |
| **3** | Port the driver: `dwc2.rs` -> the service, through the SDK's safe `Mmio`/`Dma` wrappers so the service carries no `unsafe` (§18.2) | not started |
| **4** | Retire `NET_DEVICE` (syscalls 42-44) and the `usb_disk_*` syscalls - they exist only because the driver is in-kernel. `nic-driver` and `block-driver` then talk to `dwc2` over IPC, exactly as on the Pi 4 | not started |

**Phase 2 is deliberately a skeleton.** It is the point where the fallback stops being taken, so the
first boot after it is the one where USB either works from userspace or does not work at all. Proving
the interrupt arrives BEFORE betting 3981 lines on it costs one boot and removes the largest unknown.

## What makes arm32 harder than the AArch64 port

DWC2 is far more software-driven than xHCI: split transactions, NAK retries, and a stack currently
driven from **both** the IRQ and the core-0 timer tick. A userspace service needs its own loop, and
every correlation lesson from `docs/xhci-completion-correlation.md` applies directly - one queue,
several consumers, and no way to tell whose completion is whose is the failure mode that cost the
Pi 4 a week.

## Parked nitpicks

Small, real, and deliberately not being chased mid-port. Collected here so they are not lost.

- **`dwc2: smsc95xx (LAN9514) up: ... (HW-UNVERIFIED)`** - the label is stale. That NIC gets a DHCP
  lease on the same boot that prints it, so it is hardware-verified and has been for some time. A
  label that says "unverified" about a working device teaches a reader to distrust the labels.
- **`fs`'s "20s" mount wait** was fixed (`21324670`) but the same shape - an attempt count that
  outruns a clock bound because the loop advances with `yield_cpu` - is worth grepping for elsewhere.
  `yield_cpu` does not wait; it hands the core back and leaves the task Ready.
- **`boot/pi4/config.txt` is not in the repo.** The Pi 2's is (`boot/pi2/config.txt`), so the v0.10.0
  release could ship a complete Pi 2 bundle and only a bare kernel image for the Pi 4. Capture the
  real file from a working card - do not reconstruct it from memory.
