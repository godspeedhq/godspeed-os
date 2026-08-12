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
| **2** | A skeleton `dwc2` service holding MMIO + DMA + `hw_irqs = [0x29]`, which does nothing but prove the interrupt ARRIVES in userspace | ✅ **PROVEN on hardware 2026-08-12** - see below |
| **3** | Port the driver: `dwc2.rs` -> the service, through the SDK's safe `Mmio`/`Dma` wrappers so the service carries no `unsafe` (§18.2) | **unblocked** - its premise is now measured |
| **4** | Retire `NET_DEVICE` (syscalls 42-44) and the `usb_disk_*` syscalls - they exist only because the driver is in-kernel. `nic-driver` and `block-driver` then talk to `dwc2` over IPC, exactly as on the Pi 4 | not started |

**Phase 2 is deliberately a skeleton.** It is the point where the fallback stops being taken, so the
first boot after it is the one where USB either works from userspace or does not work at all. Proving
the interrupt arrives BEFORE betting 3981 lines on it costs one boot and removes the largest unknown.

## Phase 2 result: the interrupt reaches userspace

```
12:12:06.713  ehci: kernel deliver() vector=0x29 on core 0
12:12:06.713  dwc2-svc: *** USB INTERRUPT DELIVERED TO USERSPACE *** - arm32 device IRQ routing works
              dwc2-svc: alive - 1 USB IRQ(s), 1 message(s) total
```

Same millisecond, kernel router to userspace service. **`CLAUDE.md` §6.4's justification for the
in-kernel ARM32 USB stack is empirically dead** - the port is now work not yet done, not a property
of the hardware. §6.4 needs amending once Phase 3 lands.

Exactly one interrupt, which is correct: the skeleton never unmasks, because it cannot clear the
device condition that keeps a level-triggered line asserted.

**Four boots, and only the last failure was about USB.** The other three were address arithmetic:

1. `MapFailed` - `mmio_bar()` returned a physical address, so the spawn took the PCI route and mapped
   at `XHCI_MMIO_VA` = `0x1_0000_0000`, which does not exist on a 32-bit machine. Loud, one boot.
2. **A wedge, three times** - the DMA arena mapped at `XHCI_DMA_VA` = `0x2_0000_0000`, and
   `arch/arm/page_tables.rs` opened `map()` with `virt.0 as u32`. That cast does not fail on an
   address it cannot represent; it TRUNCATES to `0x0` and maps the arena over the kernel's low
   megabyte. The watchdog then blamed the task. `map()` now returns `VirtOutOfRange`.
3. **The skeleton lied** - `let _ = ctx.recv_timeout(...)` consumed the interrupt message and
   discarded it, so the service reported `0 USB IRQ(s)` on a boot whose kernel log said `deliver()`
   had run. A discarded return value, in the counter whose whole job was to answer this question.

The lesson worth keeping: **(1) cost one boot and (2) cost three, and they are the same bug** - a
64-bit constant on a 32-bit machine. The only difference was that one path checked and the other
truncated. Both `XHCI_MMIO_VA` and `XHCI_DMA_VA` were x86_64 values wearing arch-neutral names.

## Repeat delivery: settled (2026-08-12)

The skeleton was given a throttled re-arm - sleep 1 s, then unmask - because delivering ONCE proves
nothing Phase 3 can build on. `ctx.sleep` blocks the task, so the core is free between interrupts and
the rate is bounded by construction rather than by hope, which is exactly what an immediate unmask
failed to be.

```
12:27:25   59 USB IRQ(s),  59 message(s)  (REPEAT DELIVERY WORKS)
12:28:51  145 USB IRQ(s), 145 message(s)
12:29:13  167 USB IRQ(s), 167 message(s)
12:29:45  199 USB IRQ(s), 199 message(s)
```

**86 interrupts in 86 seconds** - exactly one per second, matching `REARM_MS` to the tick. A
metronome, not a drift. No wedge, and `messages == IRQs` at every sample, so nothing is lost and
nothing spurious arrives on that endpoint.

That also settles the other open item for free: the unmask path demonstrably works 199 times, so the
earlier second-spawn `0 IRQs` was a quiet bus, not a broken unmask. Both hypotheses were live and the
throttle distinguished them without a separate experiment.

**Phase 3 is unblocked.** Its assumption - that a userspace driver can receive its controller's
interrupt repeatedly - is now measured rather than hoped.

Still unconfirmed and harmless: `observe` showed core 0 at 97% while the skeleton held the vector,
most likely the in-kernel driver polling a controller whose interrupt has been taken away. Worth a
glance during Phase 3, when that driver stops existing.

## `spawn dwc2` is one-way: reboot to get USB back

`kill dwc2` releases the route and unmasks the line, and the keyboard still does not come back.
Handing back the INTERRUPT does not hand back DEVICE STATE: while the service held the vector, the
keyboard's transfer completions were delivered to a driver that ignores them, so the in-kernel
driver's channel sits mid-transfer waiting on an event it never saw resolve, and the periodic hooks
resume polling a channel that is already stuck.

Calling the driver's own `init()` on the ownership edge was tried and **removed**. It never fired -
it sat in `wait_for_interrupt`, and core 0 does not reach idle in that window. Making it fire needs
somewhere that reliably runs plus a ~600 ms bring-up that cannot happen in a tick handler: real work,
on a recovery path for a driver Slice 5 deletes.

**So: reboot between experiments.** Twenty seconds, no code, and the serial console keeps the machine
usable throughout - the shell, `selfcheck` and the logs all work with USB down, which is what makes
Slices 1 to 4 testable at all.

## Phase 3 slicing

### What the code actually looks like

The external surface is small - eight entry points in three groups:

| Called from | Entry points |
|---|---|
| Boot + IRQ | `init`, `on_usb_irq` |
| Timer tick | `hotplug_poll`, `link_poll`, `net_rx_drain_tick`, `async_bulk_watchdog` |
| Syscalls | `msc_*` (block-driver), `net_frame_tx`/`net_frame_rx`/`net_info` (nic-driver) |

Internally it layers cleanly, and the layers are a dependency chain rather than a tangle:

```
registers + channels (~900)  ->  control transfers (~150)  ->  enumeration + hub + hot-plug (~600)
                                                                    |
                                            +-----------------------+-----------------------+
                                            |                       |                       |
                                     keyboard (~150)      mass storage (~800)      networking (~900)
```

The three device classes are **independent leaves**. That is what makes slicing possible at all.

### The constraint that shapes the slices

**The controller has exactly one owner.** It cannot be half-moved - whoever holds the IRQ drives the
hardware. So a slice cannot be "some transfers in userspace"; it has to be *"the service does
everything up to X, and past X nothing works yet."*

Each rung is therefore a working machine with FEWER DEVICES, which is testable at every step.

### The slices

| # | Work | Test |
|---|---|---|
| **0** | One owner: gate the in-kernel driver's tick hooks on `route::registered_endpoint(USB_VECTOR).is_none()` - the same predicate the IRQ dispatch already uses | ✅ selfcheck 351/0, no panics, invisible on a normal boot. **Handover is ONE-WAY - see below** |
| **1a** | Core bring-up: soft reset, host mode, FIFO sizing, root-port power + reset | ✅ **hardware-verified 2026-08-12** - `core bring-up OK`, `HPRT=0x0000100f connected=true enabled=true speed=high` |
| **1b** | Channels + control transfers (`chan_program`, `chan_dma`, `ctrl_xfer`) | ✅ **hardware-verified 2026-08-12** - `DEVICE DESCRIPTOR len=18 type=0x01 usb=0x0200 mps0=64` |
| **1c-i** | Address + identify the root device, hub descriptor | ✅ **hardware-verified 2026-08-12** - `0424:9514 class=0x09 ports=5` (the LAN9514's integrated hub) |
| **1c-ii** | Hub port survey (power, status, speed) | ✅ **hardware-verified 2026-08-12** - 4 attached, status words byte-for-byte identical to the kernel driver's |
| **1c-iii** | SPLIT TRANSACTIONS: address a device behind the hub | the same VID/PIDs the kernel driver reports for the downstream devices |
| **2** | Keyboard: HID + `CONSOLE_PUSH` | Typing works from userspace. Second ON PURPOSE - it makes the machine usable for testing the rest |
| **3** | Mass storage: BOT/SCSI; `block-driver` moves off the `usb_disk_*` syscalls to the block IPC protocol it already speaks on the Pi 4 | `drives`, `ls`, `selfcheck` |
| **4** | Networking: CDC-ECM + smsc95xx; `nic-driver` moves off `NET_DEVICE` (42-44) to frame IPC | DHCP, `ping` |
| **5** | Delete `arch/arm/dwc2.rs`, the six syscalls, the tick hooks | `chaos max-carnage` + `selfcheck`. THEN amend §6.4 |

### Two things to decide deliberately rather than inherit

- **The tick hooks are not all equal.** `hotplug_poll` and `link_poll` are genuine periodic work.
  `net_rx_drain_tick` and `async_bulk_watchdog` exist partly because a kernel driver had a tick
  conveniently to hand. In a service those become its own loop, and that is a design choice worth
  making rather than porting across unexamined.
- **Split transactions are the risk.** `split_txn_periodic` does microframe-accurate scheduling
  (`wait_for_uframe`, `write_hfnum`). It is the most timing-sensitive code in the file, and it moves
  from ring 0 with interrupts masked to a PREEMPTIBLE userspace task. Expect this to be the slice that
  bites. It sits inside Slice 1, because the hub needs it - worth knowing before rather than during.

## Slice 1a result

```
dwc2-svc: DesignWare USB 2.0 OTG core, GSNPSID=0x4f54280a
dwc2-svc: DFIFO depth 4080 words; sizing RX/NPTX/PTX 774/256/512 (Linux bcm2835)
dwc2-svc: core bring-up OK
dwc2-svc: root port HPRT=0x0000100f connected=true enabled=true speed=high
```

Connected, enabled, powered, high speed - the Pi's internal hub, brought up from USERSPACE with zero
`unsafe` in the service. First try on hardware.

`0 USB IRQ(s)` afterwards is correct and not a regression: bring-up step 1 masks global interrupts
(`GINTMSK = 0`) and nothing re-enables them yet, because there is nothing to service until channels
exist. Slice 1b re-enables them with the first transfer.

**What made it work first time was refusing to re-derive it.** The register map and sequence were
lifted with their comments, and three of the steps they carry (UTMI+ 8-bit selection, the bcm2835
FIFO layout, the post-resize FIFO flush) are HW-diagnosed facts that pass in QEMU and fail on the
board. The one thing the mechanical lift dropped - the `cfg` gate on `DMA_BUS_ALIAS` - was caught by
the compiler, and would otherwise have failed on exactly one of the two targets.

## Slice 1b result

```
dwc2-svc: DEVICE DESCRIPTOR len=18 type=0x01 usb=0x0200 mps0=64
```

Every field is right: 18 is the standard device-descriptor length, `0x01` is DEVICE, `0x0200` is USB
2.0, and `mps0=64` is a high-speed device's EP0. A real descriptor, read from the Pi's internal hub by
a USERSPACE driver.

That one line exercises the entire transfer path: channel programming, a DMA the controller performs
against the service's own granted arena, the `DMA_BUS_ALIAS` translation **on real silicon**, and all
three control stages. It also confirms the no-cache-maintenance reasoning empirically rather than by
argument - the arena is Device/uncached, so the `flush_dcache` calls the kernel driver needs for its
cached buffers have no counterpart here.

**Two slices, two first-try passes on hardware.** Both because the code was LIFTED with its comments
rather than re-derived: the sequence, the register order, the odd-frame rule and the short-packet
scratch-zeroing are all facts the board taught someone, and none of them is visible in a datasheet or
detectable in QEMU.

## Slice 1c (first half) result

```
dwc2-svc: root device addressed - VID:PID=0424:9514 class=0x09 mps0=64
dwc2-svc: USB2 hub with 5 downstream ports
dwc2-svc: ENUMERATION OK - 0424:9514 class=0x09 ports=5
```

Correct on every field. `0424:9514` is the LAN9514's integrated hub, `0x09` is the hub class, `mps0=64`
is high speed, and 5 ports is right for this part - four external sockets plus one internal port for
its own ethernet function.

**The predicted value in the previous commit was wrong, and the code was right.** I had grepped a
`downstream VID:PID` line out of a kernel log and read it as the root device: `0424:ec00` is the
SMSC95xx ETHERNET function, which hangs off one of this hub's ports. Worth recording because the
acceptance test was stated as "must match what the kernel driver reports", and a baseline taken from
the wrong line would have condemned working code. Check WHICH device a reference value describes.

## Slice 1c-ii result

```
hub port 1 CONNECTED speed=full enabled=false (status=0x0101)
hub port 2 CONNECTED speed=full enabled=false (status=0x0101)
hub port 3 CONNECTED speed=full enabled=false (status=0x0101)
hub port 4 CONNECTED speed=low  enabled=false (status=0x0301)
hub port 5 empty
hub survey complete - 4 device(s) attached, 4 need split transactions
```

Status words byte-for-byte identical to the in-kernel driver's on the same hardware. `enabled=false`
is correct rather than a fault: a port enables only after a RESET, and the survey reports rather than
binds.

**Confirmed: every device on this board needs split transactions.** Three full-speed, one low-speed,
none high-speed. There is no shortcut available - splits are not an optional extra for this port,
they are the only way to reach anything, and 1c-iii cannot be deferred or worked around.

### The bug this slice found, and why it was cheap

The first attempt reported all five ports EMPTY. The reads had not failed - they returned ZEROS,
because the hub had been addressed but never CONFIGURED, and USB 2.0 chapter 9 only permits class
requests in the CONFIGURED state. No STALL, no error, just an empty topology.

It was findable in one boot only because slice 1b zeroes the IN scratch before every read. Without
that, the survey would have reported whatever the previous transfer left in the buffer: a plausible
topology assembled from stale bytes, which is worse than an obviously wrong one because it would have
been believed. That defence was written three slices earlier for exactly this shape of bug.

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
