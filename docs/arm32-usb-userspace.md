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
| **1c-iii** | Address devices behind the hub, direct and via split | ✅ **COMPLETE 2026-08-12 - 4/4 exact match with the in-kernel driver** |
| **2a** | Find + bind a boot keyboard (config walk, SET_CONFIGURATION, SET_PROTOCOL) | ✅ **hardware-verified 2026-08-12** - `interface 0 endpoint 1 mps 8 interval 10`, matching the kernel driver |
| **2b** | POLL the interrupt endpoint via a PERIODIC split, push to `CONSOLE_PUSH` | typing works from userspace |
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

## Slice 2a result

```
dwc2-svc: BOOT KEYBOARD bound - interface 0 endpoint 1 mps 8 interval 10
```

Matches the in-kernel driver's own binding (`mps=8 interval=10`). Ports 1-3 are silent, correctly -
they are not keyboards, which is an ordinary outcome and not a failure.

### What 2b has to face, and what is now known about it

`interval 10` is the number the periodic scheduling needs, and it is now measured rather than
assumed. What remains is the ONE risk this port has carried from the start:

- a PERIODIC split is microframe-scheduled - `split_txn_periodic`, `wait_for_uframe`, `write_hfnum` -
  where the non-periodic split used for enumeration merely SWEEPS microframes across retries and is
  therefore tolerant of bad timing;
- that code moves from ring 0 with interrupts masked into a PREEMPTIBLE userspace task, and a
  preemption in the wrong microframe does not fail cleanly. It transfers nothing.

Everything underneath it is now proven: the controller, the channels, control transfers direct and
split, enumeration, addressing, the hub, and the keyboard's own binding. So when 2b misbehaves it
will misbehave for one reason, which is the whole point of having got here in slices.

**Read `split_txn_periodic` before writing it.** That has been the cheapest move five times in this
port - most recently the multi-packet split, where the answer was in a comment written by someone who
had already paid for the same symptom on the same board.

## SLICE 1 COMPLETE

```
port 1 DEVICE direct    - VID:PID=0424:ec00 class=0xff speed=high addr=2   SMSC ethernet
port 2 DEVICE direct    - VID:PID=0781:5567 class=0x00 speed=high addr=3   SanDisk stick
port 3 DEVICE direct    - VID:PID=0bda:8176 class=0x00 speed=high addr=4   Realtek
port 4 DEVICE via split - VID:PID=046d:c30a class=0x00 speed=low  addr=5   Logitech keyboard
```

**Four of four, matching the in-kernel driver exactly.** A userspace service now resets the DWC2,
brings up the root port, runs control transfers, enumerates and configures the hub, surveys and
resets its ports, assigns addresses, and reads full device descriptors - three devices directly and
one through a transaction translator. Zero `unsafe` in `services/dwc2/`, and no cache maintenance,
both because of what the grant is rather than by working around anything.

### The four bugs, and what found each

| Bug | Found by |
|---|---|
| Splits sent to HIGH-speed devices (speed bits are invalid until a port RESET) | trying all four ports instead of stopping at the first failure - four devices do not fail identically |
| No `SET_CONFIGURATION` - hub answered class requests with zeros | zeroed IN scratch: an obviously-empty topology instead of a plausible one |
| Only 8 bytes read, VID lives at 8..11 | zeroed IN scratch again, plus a CORRECT class byte at offset 4 proving the transfer worked |
| No addresses assigned - two devices answering at address 0 | the ORDERING: ports 2-3 failed only AFTER port 1 succeeded |
| Multi-packet split not sequenced per packet | reading the kernel driver, whose comment describes this exact symptom on this exact board |

Three of those were caught by one decision made three slices earlier - zeroing the IN scratch before
every read - which turned "plausible data assembled from stale bytes" into "obvious zeros" every
time. The defence was written for a hub port-status short read; it paid for itself on three unrelated
bugs.

## Superseded: slice 1c-iii in progress

```
port 1 DEVICE direct    - VID:PID=0424:ec00 class=0xff speed=high addr=2
port 2 DEVICE direct    - VID:PID=0781:5567 class=0x00 speed=high addr=3
port 3 DEVICE direct    - VID:PID=0bda:8176 class=0x00 speed=high addr=4
port 4 DEVICE via split - VID:PID=0000:0000 class=0x00 speed=low  addr=5
```

Three of four match the in-kernel driver EXACTLY. Enumeration of directly-attached devices is done.

**What remains is narrow and precisely bounded.** Port 4 (low speed, behind the transaction
translator):

- its **8-byte** descriptor read SUCCEEDED - the code bails on `mps0 == 0` and did not;
- `SET_ADDRESS` through the split SUCCEEDED;
- `class=0x00` is genuinely correct for a HID keyboard (its class lives at interface level);
- only the **18-byte** read came back zeros.

At MPS 8 an 18-byte transfer is THREE packets where the 8-byte one was one. So single-packet split
transfers work in both directions, and the failure is specific to a MULTI-PACKET split IN. That is a
known-harder case: each packet needs its own start-split/complete-split pair, and `stage_split`
currently issues one pair for the whole transfer.

Next step, and it is a comparison rather than a guess: the kernel driver reads full descriptors from
this same low-speed keyboard successfully, so the working answer is in `split_txn` /
`chan_dma` - specifically how the packet count in HCTSIZ interacts with a split, and whether the
controller re-issues the split per packet or the driver must. Read that before changing anything.

### How the earlier STALL was resolved

The STALL was neither the hub nor the device: it was a split sent to a device that did not need one.
A port's SPEED BITS ARE NOT VALID UNTIL THE PORT HAS BEEN RESET, and the survey read them from an
idle port, announcing "4 devices, 4 need split transactions" - confidently and wrongly. Three are
high speed and are addressed directly; a hub STALLs a split aimed at a high-speed device. Port 4, the
one genuinely low-speed device, enumerated through a split on the very boot that STALLed the others.

A measurement taken at the wrong moment is worse than none, because it gets believed - it was the
input to three boots of hunting a bug in split code that was correct throughout.

## Superseded: the field-narrowing that got there

The split machinery WORKS. The transaction translator ACKs the start-split - a complete-split is only
ever issued after it does - so `hcsplt` encoding, SSPLIT/CSPLIT sequencing and the microframe sweep
are all doing their jobs. The failure is one layer further out:

```
SETUP complete-split STALLed - HCINT=0x0000000a HCSPLT=0x8001c081 HCCHAR=0x00100008 HFNUM=0x0671
```

**Both register encodings are CORRECT**, which is what the dump was for:

| | Decoded | Verdict |
|---|---|---|
| `HCSPLT 0x8001c081` | port 1, hub addr 1, XactPos=0b11 (ALL), CompSplt=1, SplEna=1 | right |
| `HCCHAR 0x00100008` | MPS 8, EP 0, dir OUT, LSpdDev=0 (full speed, matches the port), EPType 0 (control), MC 1, DevAddr 0 | right |
| `HCINT 0x0a` | CHHLTD + STALL | a device really did answer, and STALLed |

That eliminates the two most likely suspects. It also sharpens the contradiction rather than
resolving it: **a compliant device may not STALL a SETUP** (USB 2.0 8.5.3), yet something is
answering at address 0 and doing exactly that.

Refuted so far, on hardware: the split encoding, the channel programming, and a missing TRSTRCY
recovery delay (added, 15 ms, did not change the result - keep it, it is required regardless).

**Where to look next, in order:**

1. **Is the STALL from the DEVICE or the HUB?** A hub returns STALL for a request it cannot forward.
   The TT ACKed the start-split, but that only says it accepted the token, not that the downstream
   transaction succeeded. Reading the hub's port status and its TT state after the failure would
   separate them, and they want completely different fixes.
2. **Do the downstream devices still hold addresses from the IN-KERNEL driver's enumeration?** It
   enumerates everything at boot; the service then reset the CONTROLLER and the hub, but a device
   only returns to address 0 on its own PORT reset. `reset_port` does that - verify it actually took
   effect by reading the port status immediately before the transfer, rather than trusting the reset
   path's own report.
3. **`ctx.sleep` granularity.** Sub-quantum sleeps floor to a 10 ms tick on this port, so the 15 ms
   recovery is really 10-20 ms and the reset poll may be coarser than USB's timings assume. Measure
   before assuming it is fine.

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
