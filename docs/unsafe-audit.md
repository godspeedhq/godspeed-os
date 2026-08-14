# Unsafe Audit (§18.4)

`scripts/unsafe_check.py` runs on every CI push. It counts non-comment lines
containing the `unsafe` keyword per file and compares to the baseline table below.

**A PR that increases any file's count, or adds unsafe to a new file, fails CI
unless this file is updated in the same commit with a written SAFETY argument.**

`unsafe_check.py` scans `kernel/src/` (tracked against the inventory below) **and `services/`** (where
it fails on ANY `unsafe` line - §18.2 forbids service `unsafe`). The SDK's permitted-layer `unsafe`
(`syscall`, `mmio`, `dma`, `adversarial` - §18.1) is not inventoried here; each block carries a SAFETY
comment.

---

## 2026-08-04 - Pi 4 milestone 22: the display (feat/pi4-aarch64)

The Pi has no bootloader-supplied framebuffer descriptor the way x86 does with Limine, so the ARM asks
the VideoCore for one through the same property mailbox that reported the board revision and the RAM
size at milestone 7 - and in the same caches-off window, for the same reason: the GPU reads the request
straight out of RAM, so asking before the caches are on removes the coherency question instead of
answering it.

**The console itself is arch-neutral and stays `unsafe`-free.** `crate::fbcon` - the ANSI parser, the
UTF-8 decoder, the character grid, glyph rendering, scrolling - is shared with x86 and the 32-bit ARM
port. This arch owes it exactly three things (`fb_commit`, `FB_READBACK_CHEAP`, and the framebuffer as a
`&'static mut [u8]`) and gets a full terminal for them. Handing over a **slice** rather than a base
address is what keeps every pixel write in the neutral console bounds-checked: the one `unsafe` that
buys the display lives here in `arch/`, where the mapping's validity is actually known.

Three checks stand between the GPU's reply and that slice, because a mailbox reply is firmware-supplied
input and a plausible-looking wrong answer is the failure mode: the bus-address alias is masked back to
an ARM physical address (skipping that yields an address that points nowhere and a display that stays
dark with no error); the geometry is range-checked before it drives a fill loop and a slice length; and
the framebuffer must lie inside the kernel's direct map AND below `mmu::DEVICE_BASE`, since everything
above that is mapped Device-nGnRnE, which forbids the unaligned accesses the glyph renderer makes. Each
failure keeps the machine on serial and says so, rather than mapping something and faulting later
somewhere that names neither the GPU nor the map (§26.7).

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/video.rs` | new, 2 | `fb_commit`'s cache clean over the written rectangle (`dc cvac` by VA - the framebuffer is cacheable and the GPU reads RAM directly, not through the CPU's caches, so it would otherwise scan out stale pixels); and the one slice construction in `start_console`, over a framebuffer the GPU allocated and owns nothing else in, whose length is its own bounds-checked `pitch * height`, at a direct-map address checked to be inside the map. |
| `arch/aarch64/mailbox.rs` | 3 -> 4 (+1) | `property_call`: copies an arbitrary tag request into this module's 16-byte-aligned static and the reply back out, length-checked against the buffer at both ends. The static rather than the caller's array because the mailbox packs the channel into the low 4 bits of the address it is handed - an arbitrarily-aligned caller buffer would send the message to the wrong channel, so the alignment guarantee stays in one place. |
| `arch/aarch64/mod.rs` | 53 -> 55 (+2) | The write and the read of `FB_INFO`, the static that carries the geometry across the jump to the high half. A static rather than a local because the jump abandons the low stack along with every local on it - the same reason `memmap::current_map` exists. Single-threaded boot: written once before the jump, read once after. |

## 2026-08-04 - Pi 4: the serial lock made the machine crawl, and is now try-once (feat/pi4-aarch64)

The console lock below fixed the shredding and broke the machine. Boot went from seconds to **104
seconds**, characters trickled out one at a time, and `chaos` ended in a liveness-watchdog panic.

Two mistakes compounded. The claim **spun** up to two million times, and it was **held across the
framebuffer render** - glyph drawing plus cache maintenance over a rectangle, far more expensive than
the UART write it was protecting. So every core queued behind every other core's rendering, and a core
that had spent ten seconds spinning was correctly declared wedged.

Now: **one attempt, never a spin.** A contended writer emits its bytes anyway and skips only the display
mirror - the expensive part, and the part nobody reads during contention. Lines can interleave under
load, which is exactly the trade the 32-bit port makes and for exactly this reason. A log line must
never make a core wait.

Boot is back to 8 seconds with four cores on every run, and a carnage run survives.

No new `unsafe`.

## 2026-08-04 - Pi 4: the serial console had no lock, and four cores made that matter (feat/pi4-aarch64)

Removing PSCI got the board past the release: all three secondaries check in, the machine boots, reboots
and reads files back. But it reported **three** cores where QEMU reported four, and printed **no**
"core N online" lines at all - while QEMU printed every one.

The cores were running. The evidence was not surviving. Four cores wrote a byte at a time into one
UART with no serialisation, so lines were shredded into each other at character granularity. Debugging
SMP through that means drawing conclusions from an instrument that destroys the measurement, which is
worse than having none - it looks like a result.

Three fixes, each of which was necessary and none of which was sufficient alone:

1. **A claim on the log path.** Bounded and never fatal: a core that cannot get it writes anyway rather
   than spinning forever, because this path is reachable from panics and ISRs and a core deadlocked
   while reporting why is worse than a garbled line.
2. **The same claim on `put_str`.** Locking one writer and not the other leaves exactly the hole it was
   meant to close - the boot core's own lines were still being shredded by an AP's `kprintln`.
3. **The line has to BE one write.** `kprintln!` flushes as it formats, so a line assembled from
   fragments is several writes with gaps between them. The lock was never the missing piece. The report
   is now rendered into a fixed buffer and emitted once.

Five consecutive boots now report `cores in the scheduler: 4 (mask 0xf)` intact.

No new `unsafe`.

## 2026-08-04 - Pi 4 GENET milestone 3: the receive ring (feat/pi4-aarch64)

Allocates 32 receive buffers of 2 KiB, writes their PHYSICAL addresses into the descriptor area,
programs the ring geometry on queue 16, and reads it back. **Receive is deliberately left DISABLED.**

The split is the point. Every address handed over here is one the MAC will eventually write into, on a
board with no IOMMU behind it - so "the values reached the hardware" and "the hardware is running" are
separated, and only the first is claimed. The readback is what distinguishes them.

Two details that would each corrupt memory rather than return a wrong value:

- The descriptor address words are **physical**. The kernel's own view of that memory is the direct-map
  alias, and handing a virtual address to a bus master is a device writing to an address that means
  nothing to it.
- `DMA_RING_BUF_SIZE` packs the descriptor COUNT in the upper half and the buffer LENGTH in the lower -
  two different units in one register, and a swap reads back plausibly.

Ring size is 32 rather than Linux's 256: 64 KiB of pinned memory, a bound visible in the source rather
than inferred (§26.6.1).

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 3 -> 4 (+1) | Recording each buffer's physical address in this module's own array, indexed inside its length during single-threaded boot, so the descriptors and the eventual drain path agree on one answer. |

## 2026-08-04 - Pi 4 GENET milestone 4: transmit ring and the receive drain (feat/pi4-aarch64)

Frames now arrive (the RGMII block had never been enabled, so the MAC could negotiate with the PHY
over MDIO and hear nothing from it). This adds the transmit half and the drain that recycles receive
descriptors, so the whole path is exercised in a single boot rather than one hypothesis per card swap.

The five new blocks are all the same two shapes, and both are the shapes this file already had:

- **This module's own `static mut` arrays and cursor** (`TX_BUFS`, `TX_NEXT`, `RX_BUFS`), indexed
  inside their length during single-threaded boot. Identical to the `RX_BUFS` entry above.
- **One copy through the direct map** in `transmit`: the frame is written to a physical frame handed
  back by the frame allocator, addressed through `KERNEL_VA_BASE + phys`, bounded by an explicit
  length check against `RX_BUF_LENGTH` before the copy.

Worth recording because it is a portability trap rather than a soundness one: both directions need
`dma_sync` (`dc civac`). AArch64 DMA is **not** cache coherent (SEC-28), so a transmitted frame that
exists only in this core's cache is sent as stale bytes, and a received frame read without an
invalidate is read as whatever was cached before. x86 needs neither and would hide both.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 4 -> 9 (+5) | Transmit buffer table and cursor, the receive buffer lookup in the drain, and one bounded copy through the direct map into a DMA frame. Same two shapes as the existing entries; each block carries its own SAFETY comment. |

## 2026-08-05 - Pi 4: BOT reset recovery for a wedged bulk endpoint

A bulk transfer that times out leaves the endpoint HALTED and its transfer ring desynchronised, and
re-issuing the command can never clear that: 32 seconds of retries produced 106 identical timeouts,
every one asking a stopped endpoint to run. Recovery repairs both layers - xHCI `Reset Endpoint` +
`Set TR Dequeue Pointer` (the controller half, which the Pi 2's DWC2 driver has no equivalent of), then
the Bulk-Only class reset and Clear-Feature HALT on both endpoints (the device half, for the uncollected
CSW the device waits on before accepting any new CBW). Keyed on a RUN of timeouts, reset by any success,
so a merely-slow device never triggers it.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 37 -> 38 (+1) | Zeroing the two bulk transfer rings before republishing their dequeue pointers - this driver's own DMA pages, 4 KiB each, while it holds the USB claim. |

## 2026-08-05 - Pi 4: kernel-stack guard pages (block splitting)

`install_kstack_guards` was inert on this arch twice over: the neutral `main.rs` call site is not on the
Pi 4 boot path, and both arch primitives were stubs. So 224 kernel stacks sat end to end with nothing
between them, and an overflow corrupted the neighbouring task's stack instead of faulting - the exact
failure guard pages exist to make loud.

The high half is mapped with 2 MiB BLOCKS, so a 4 KiB hole cannot simply be cleared: the containing
block is SPLIT into an L3 table whose 512 page descriptors reproduce it verbatim (address, memory type,
shareability, AP, AF and the execute-never bits all carried across), the L2 entry is repointed, and then
the single guard entry is cleared. About thirty guards share each block, so the split happens once per
block rather than per guard.

Tables come from a fixed 12-entry arena in `.bss`, not a heap (§26.6.1) - the pool spans at most nine
2 MiB blocks - and exhaustion is REPORTED, because a guard page that failed to install is indistinguishable
from one that worked until something overflows.

Two long-descriptor details worth their comments, both of which read wrong at a glance: `0b11` means
TABLE at L1/L2 and PAGE at L3, and the address field is bits [47:21] for a block but [47:12] for a page.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | 17 -> 23 (+6) | High-half L2 lookup, split-table arena hand-out, the block split and page clear, the verify read, and TLB maintenance. All page-table work, in the layer where it belongs (§18.1); the neutral caller stays a safe `fn`. |

## 2026-08-05 - Pi 4: route device IRQs to userspace (Commandment I remediation, step 1)

The Pi 4 kernel contains a USB stack and an Ethernet driver, which expands kernel responsibility and
breaks Commandment I. The justification on record - "the arch does not yet route device IRQs to
userspace" - turned out to be an UNIMPLEMENTED BRANCH, not a hardware limit: the GIC-400 delivers SPIs
perfectly well, the neutral `interrupt::route` is arch-agnostic and complete, and both ARM arches already
carry the stubs it needs. Nobody had connected the two.

One branch in the GIC dispatcher now hands SPIs (ID 32+) to the neutral router, which is the mechanism a
userspace driver blocks on. IDs above 255 are retired with a one-shot loud line rather than truncated
into the wrong routing slot, because the neutral table is keyed by a `u8` vector.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | 12 -> 13 (+1) | `interrupt::route::deliver(id)` from the IRQ handler, called with interrupts masked - the same contract the x86 caller documents. |

## 2026-08-05 - Pi 4 AUDIT 8 follow-up: reclaim a departed device's DMA pages

`dma_page` never had a counterpart, so this driver freed nothing. Every unplug/replug cycle leaked the
departing device's pages - the keyboard's interrupt ring, EP0 ring and report buffer; the disk's two
bulk rings, EP0 ring, data page and command page. Four to six pages per cycle, forever, from an
ordinary operator action and precisely the one `stand_down` exists to handle.

`free_dma_page` returns one, and `stand_down` now takes the device by value before dropping it, so
nothing can still name the pages when they go back to the allocator.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 38 -> 42 (+4) | One `free_frame` wrapper, and its use on the disk and keyboard teardown paths - each guarded by the device being taken out of its option first, so no descriptor, ring pointer or endpoint context reaches the page. |

## 2026-08-05 - Pi 4 AUDIT 8: RX buffers cleaned and zeroed before the device owns them

Audit 8 (four parallel auditors) found that GENET hands the controller RX buffers with no cache
maintenance at all. `alloc_frame` neither zeroes nor cleans, and `allocator_selftest` writes read-back
patterns into frames and frees them immediately before this driver probes - so a buffer can arrive
carrying DIRTY lines. The device DMAs into that physical memory behind the cache, and the read side's
`dma_sync` is `dc civac`: clean THEN invalidate. The clean writes the stale line back OVER the frame the
controller just delivered. The operation intended to make the DMA visible is the one that corrupts it -
intermittently, and invisibly under QEMU.

Zeroing the buffer at ring build closes a second issue in the same line: `receive_one` trusts the
controller's length for how many bytes are MEANINGFUL, so a device reporting more than it wrote would
hand a `NET_DEVICE` holder whatever kernel data previously occupied the frame (the SEC-21 class).

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 13 -> 14 (+1) | `write_bytes` zeroing each RX buffer before it is handed to the controller, paired with a `dma_sync` clean so no dirty line can be written back over delivered data. |

## 2026-08-05 - Pi 4 AUDIT: the barrier class, swept (SEC-25/26/27)

Auditing by DEFECT CLASS rather than by file, after the respawn fault turned out to be one missing
barrier. The question asked of every site: where aarch64 writes a page-table descriptor, is it published
before something can walk it, and where one is REMOVED, is the translation invalidated before the frame
is reused?

Barrier coverage was already correct in `mmu::enable`, `enable_secondary`, `drop_low_map`,
`invalidate_tlb_page`, `switch_context`, `free_page_table_root` and the guard-page unmap. The gap was
`ptables.rs` - the file the neutral kernel actually maps and unmaps through - which had **no barriers at
all**. Three findings, all one class:

1. `finalize_service_address_space` (previous entry) - a new address space installed before its
   descriptors were visible. The supervisor-respawn fault.
2. `map_in_root` - called by `map_in_active_tables` against THIS CORE'S LIVE TABLES, so the walker can
   reach a descriptor that has not become visible. Same defect, tighter window.
3. `unmap` - cleared the descriptor and handed the `Frame` back to the allocator with no barrier and no
   TLB invalidate. The previous owner can then read and write a frame that belongs to another address
   space: a use-after-free the MMU actively enables, reported by nothing. x86 does not need it here
   because its neutral kill path shoots down; that is exactly why the omission reads as complete.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/ptables.rs` | 20 -> 21 (+1) | `dsb ishst` publishing descriptors in `map_in_root`, and `dsb ishst` + `tlbi vaae1is` + `dsb ish` + `isb` in `unmap` before the frame is released. |

## 2026-08-05 - Pi 4 AUDIT: publish page tables before the walker can reach them (SEC-25/27)

First finding of the aarch64 kernel audit, and the cause of the supervisor-respawn fault that the
earlier TLB fix reduced but did not cure.

`ptables.rs` builds a task's address space with ordinary stores and contains **zero barriers** - grep
finds not one `dsb` in the file. On x86 that is free, because page-table walks are strongly ordered. On
AArch64 the entries are plain memory writes with nothing ordering them against the TTBR0 install that
follows, so the hardware walker can observe a descriptor this core has already written as absent, and
raise a translation fault for a page that is mapped.

That is the fault signature exactly: ESR `0x82000007`, instruction abort on the first fetch of a
freshly-spawned supervisor whose 69 pages were all mapped correctly. Boot spawns survive because other
work (and its incidental barriers) separates building a space from entering it; a respawn does both back
to back, which is why the respawn is the one that dies and why it is intermittent.

## 2026-08-11 - Pi 2: a panic must halt every core there too (kernel audit A11-2)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 44 -> 45 (+1) | `park_core_forever`: `cpsid if` then a `wfi` loop. `halt_all_cores` was `loop { spin_loop() }` - masking nothing and signalling nobody - on a port running four cores on real hardware, so a panic left the other three scheduling and the panicking core taking timer IRQs. Masking IRQ+FIQ and halting is always valid in a privileged mode, and the function never returns, so nothing is left inconsistent. Touches no memory. |

## 2026-08-09 - Pi 4: a panic must halt every core (kernel audit A10-1)

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 67 -> 68 (+1) | `park_core_forever`: `msr daifset, #0xf` then a `wfi` loop. `halt_all_cores` was `loop { spin_loop() }`, which masked nothing and stopped nobody - the panicking core kept taking the timer IRQ and was scheduled away, and the other cores never learned. Masking DAIF and halting at EL1 is always valid, and the function never returns, so no state is left inconsistent by the mask. Touches no memory. |

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 66 -> 67 (+1) | `dsb ishst` in `finalize_service_address_space`, publishing a new address space's descriptors inner-shareable before it can be installed or walked. A barrier; touches no memory. |

## 2026-08-05 - Pi 4: invalidate a dead address space's TLB before its frames are recycled

`chaos kill-storm supervisor` killed the machine on this port: the RESPAWNED supervisor took an
instruction abort on its first fetch (ESR `0x82000007`, translation fault level 3) and wedged core 0
hard enough that the other three raised the liveness panic. Seven boot spawns were fine; the first
respawn died.

`switch_context` skips installing TTBR0 when the incoming base equals the outgoing one. That is sound
only while a TTBR value identifies an address space, and it stops identifying one as soon as root
frames are recycled - the allocator hands a just-freed frame straight back, so a service that dies and
respawns can get the SAME root physical address with entirely different contents. The switch is elided
as a no-op and the new task runs on the dead task's mappings, whose frames have already been reclaimed.

One `tlbi vmalle1is` in `free_page_table_root`, before the frames are handed back. Inner-shareable on
purpose: the dying task's entries may live in a core other than the one running the kill, and a local
`vmalle1` would leave them there. This is the SEC-26/SEC-27 obligation in `arch/CLAUDE.md` - an
AArch64 address-space change does not implicitly flush - met at the point where the invariant actually
breaks rather than by flushing on every context switch.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 65 -> 66 (+1) | TLB maintenance (`tlbi vmalle1is`) at address-space teardown, so a recycled root frame cannot inherit the dead space's translations. |

## 2026-08-05 - Pi 4 GENET milestone 5: the NET_DEVICE bridge (feat/pi4-aarch64)

Wires the working driver to the arch-neutral `net_frame_tx` / `net_frame_rx` / `net_info` seam, so the
unchanged userspace `nic-driver` and net-stack can drive it. Four new blocks, same shapes again: the
station-address store, and one bounded copy OUT of a DMA buffer in `receive_one`.

One is worth stating explicitly because it is the only place device-supplied data sizes a copy. The
frame length in `receive_one` comes out of a descriptor **the controller wrote**, so it is untrusted
input on a path reachable from userspace via a syscall. It is clamped to BOTH the caller's slice length
and `RX_BUF_LENGTH` before the copy, so a controller reporting a nonsense length can overrun neither
the destination nor the source buffer. `GENET_READY` gates every one of these entry points, so a board
where the controller was absent or a ring refused to program cannot be driven into uninitialised
registers from userspace at all.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 9 -> 13 (+4) | Station-address store (written once before the Release that publishes readiness), the receive buffer lookup, and one copy out of a DMA buffer whose length is device-supplied and clamped to both source and destination before use. |

## 2026-08-04 - Pi 4 GENET milestone 2: the MAC and the MDIO bus (feat/pi4-aarch64)

Resets the UniMAC, programs the station address and frame limit, selects the external gigabit PHY mode,
and reads the PHY's identity over MDIO. The data path stays OFF: this brings up the MAC, not the rings,
and a receiver enabled with no ring behind it fills a FIFO nobody drains.

The PHY id is the milestone worth reaching. A real id means three separate things are true at once -
the MAC is alive, the MDIO bus is clocking, and the PHY is answering - and a later DMA failure would
otherwise be blamed for any of them.

Three details that would each produce a plausible-looking wrong result:

- **`UMAC_CMD` and friends are not in `bcmgenet.h`.** Modern Linux moved the UniMAC registers to a
  shared `unimac.h`. Searching the obvious header finds nothing, and the obvious conclusion - that this
  part has no such register - is wrong.
- **MDIO `READ_FAIL` must be checked.** A read of an absent PHY returns a perfectly plausible `0xFFFF`
  with that bit set; a driver that checks only the data concludes the PHY answered with every
  capability enabled.
- **The link bit latches low.** A single read of the basic status register reports a link that has been
  up all along as DOWN, so it is read twice.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 1 -> 3 (+2) | The register read and write helpers - volatile accesses inside the kernel's Device mapping, to a controller whose presence the milestone-1 probe already established. |

## 2026-08-04 - Pi 4 GENET milestone 1: find the controller and identify it (feat/pi4-aarch64)

The Pi 2 has no Ethernet MAC - its port hangs off a LAN9514 behind the USB hub, which is why that
port's networking rides the in-kernel DWC2 stack. The Pi 4 has a real MAC on the SoC (GENET v5 at
`0xFD58_0000`) with its own DMA rings and an external RGMII PHY over MDIO. None of the Pi 2's network
path transfers; only the seam above it does.

This milestone reads `SYS_REV_CTRL` and reports the revision, and stops there deliberately. It proves
the MMIO window decodes **before** any driver above it can blame its own logic for a dead read - the
PCIe bring-up on this board spent four hardware rounds on a window that was silently not forwarding,
and every one of them was spent looking at the driver. The revision also selects the register layout:
v1..v5 move the DMA rings, so a driver written against v5 offsets on a v3 part reads plausible values
from the wrong places.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | new, 1 | The revision read, through `probe_read32` so an absent controller is reported rather than taken as an external abort that surfaces later blaming something unrelated. |

## 2026-08-07 - GENET's interrupt routed to userspace (feat/pi4-aarch64)

The third `route::deliver` call in this file, one per interrupt this port hands to a userspace
driver, all under the same contract: called from the IRQ handler with interrupts masked. GENET's is
registered LEVEL-triggered, so `deliver` masks the source before handing it over - the driver clears
`INTRL2_0` and unmasks with the `IrqUnmask` syscall.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | 14 -> 15 (+1) | `route::deliver` for GENET's macirq (SPI 157), translated to the neutral vector `nic-driver` is granted. Same contract as the MSI and generic SPI arms beside it. |

## 2026-08-07 - GIC disable: masking a level-triggered device IRQ for a userspace driver (feat/pi4-aarch64)

A LEVEL-triggered device interrupt keeps its line asserted until the DEVICE's own status register is
cleared - and on this port that register lives in MMIO only the userspace driver maps. So the kernel
cannot acknowledge it; it can only stop listening until the driver says it has (the `IrqUnmask`
syscall). Without `disable`, `route::deliver` returns with the line still high and the interrupt
re-enters immediately, which the liveness watchdog turns into a panic.

`GICD_ICENABLER` is write-1-to-clear, so the write touches only the named interrupt - a
read-modify-write would race another core enabling a different one.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/gic.rs` | 6 -> 7 (+1) | `disable(id)`: one volatile write to `GICD_ICENABLER`, the counterpart of the existing `enable`. Same Device mapping, same bounds argument. |

## 2026-08-07 - MSI: the USB driver waits on an interrupt instead of a timer (feat/pi4-aarch64)

The Pi 4's xHCI service could only find an event by waking on a timer and looking - the controller's
MSIs were masked at PCIe bring-up ("interrupts are not routed yet - the event ring is polled"). That
is a floor on input latency no tuning removes, because the wait is not waiting FOR anything.

The BCM2711 raises ONE SPI for all 32 of its MSIs, so the handler is a demultiplexer: read
`MSI_INTR2_STATUS`, clear it, deliver. The one added `unsafe` is the existing `route::deliver` call
shape, in the arm that handles that SPI - the same call, under the same documented contract (IRQ
handler, interrupts masked), as the generic SPI arm two lines below it.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | 13 -> 14 (+1) | `route::deliver` for the demultiplexed PCIe MSI. Identical contract to the generic SPI delivery beside it: called from the IRQ handler with interrupts masked. |

## 2026-08-06 - the in-kernel GENET driver is DELETED: 14 unsafe lines to 1 (feat/pi4-aarch64)

The largest single reduction in this audit's history, and it is a reduction because the code did not
move to a safer place in the kernel - it left the kernel entirely. `arch/aarch64/genet.rs` was a
~1500-line Ethernet driver (MDIO, UMAC bring-up, PHY clock delays, RX/TX descriptor rings, the address
filter, the frame data path) holding 14 audited unsafe lines. It is now 113 lines that read one
revision register and write none.

The driver lives in `services/nic-driver/src/genet.rs`, a restartable userspace service with **zero**
unsafe. That is not a coincidence of style: a service reaches its controller through an MMIO capability
and a DMA arena, both handed to it by name at spawn, and the SDK's `Mmio`/`Dma` wrappers are the only
things that touch a raw address. The unsafe did not get rewritten more carefully - the *need* for it
was removed by putting the driver on the other side of the capability boundary (§18.1, §18.2).

Worth stating plainly because it is the point of the whole exercise: those 14 lines parsed frames that
arrive unbidden from anywhere on the network, in ring 0. The new driver parses the same frames in a
task that chaos killed 46 times during a carnage run while the machine stayed up.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/genet.rs` | 14 -> 1 (-13) | The driver is deleted. The one line left is the milestone-1 revision read through `probe_read32` - discovery, which the kernel owes (it gates the register-window grant), not driving, which it does not. |

## 2026-08-04 - Pi 4 SMP: the PSCI attempt hung the board, and is removed (feat/pi4-aarch64)

Four cores came up on six consecutive QEMU boots and the board hung on the release line. The cause was
the PSCI attempt, and the guard around it was the wrong claim.

**An `smc` with no EL3 handler does not return an error - it traps to a vector nobody populated and the
machine stops dead**, before it can report anything. There is also no way to ask first: a PSCI version
query is itself an `smc`, so probing has exactly the failure it is probing for.

The guard was "we booted at EL2, so something must own EL3". That is not the same claim. The stock Pi 4
armstub passes control at EL2 and implements **no PSCI handler at all**. QEMU, which hands over at EL3,
was the only environment where the guard did the right thing - so the bug was invisible in the one place
it was tested, and the fix that made QEMU pass is what made hardware hang.

The spin table is what this firmware implements, what the board's own device tree describes, and it
needs no `smc` to find out. It is now the only mechanism.

No new `unsafe` (the PSCI `smc` block is gone; the count is unchanged because the spin-table write and
the cache clean it needs were already there).

## 2026-08-04 - Pi 4 SMP: four cores, on by default (feat/pi4-aarch64)

The last piece was a **single missing line**, and the 32-bit port had it all along
(`arch/arm/mod.rs`: `set_core_lapic_id(core_id, core_id)`).

`lapic_to_core_id` resolves a core by searching the id table and, finding nothing, **returns 0**. So a
core that never registers itself does not fail - it silently answers "I am core 0" and starts using core
0's run queue, scheduler context and per-core state. The fallback is correct for exactly one core, which
is why nothing showed until the other three arrived, and why the symptom was an intermittent hang rather
than an error.

Six consecutive QEMU boots now reach a shell on four cores, and `chaos max-carnage` survives 6 rounds /
18 kills with the kernel alive. Enabled by default; the feature remains separate so a single-core image
is one flag away if a hardware fault ever needs bisecting against it.

No new `unsafe`.

## 2026-08-04 - Pi 4 SMP: two real bugs found by reading the 32-bit port (feat/pi4-aarch64)

Still gated off and still not reliable (one boot in five reaches a shell), but two genuine defects were
found, and the second only because the working port was compared against rather than reasoned from.

**The flag the secondaries poll was never cleaned to memory.** The boot core stores it through a
cacheable mapping; a parked core reads it with translation and caches OFF, straight out of physical
memory. Nothing forces the line out, so whether the cores ever started depended on when the cache
happened to evict it. The spin-table write two lines away already did this - the same lesson applied to
one of the two places it applies to. With the clean, three of three check in on every boot.

**`get_lapic_id` returned a hardcoded 0.** The neutral `scheduler::current_core_id` is built on it, so
the moment the other cores came up, every one of them believed it was core 0 - four cores sharing one
run queue, one scheduler context, one set of per-core state. With a single core it was indistinguishable
from correct, which is why it survived twenty-odd milestones. The 32-bit port has always returned
`mpidr & 3` there.

Also learned by comparison and worth recording: the 32-bit port runs four cores with **both IPI senders
still empty stubs**. Cross-core IPIs are a latency improvement, not a correctness requirement - every
core ticks on its own timer and picks up work then. The SGI path added here is therefore a nicety, and
the remaining flakiness is somewhere else.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/gic.rs` | 5 -> 6 (+1) | `send_sgi`: the distributor's SGI register. Writing it raises an interrupt on the targeted cores and does nothing else. |
| `arch/aarch64/mod.rs` | 64 -> 65 (+1) | `get_lapic_id`: an `MPIDR_EL1` read, side-effect free. |

## 2026-08-04 - Pi 4: secondary cores, behind a feature and NOT working yet (feat/pi4-aarch64)

`pi4-smp` releases the other three cores. It is **gated off and it is not finished**: one QEMU boot
reaches all four cores and a shell, the next hangs at the release. An intermittent race is worse than a
missing feature, so the default image is unchanged.

Two findings are worth keeping regardless, because both are traps rather than bugs:

- **An `smc` is only safe if something still owns EL3.** When the firmware hands this kernel control AT
  EL3 - which is what QEMU does - the kernel performs its own drop and leaves no handler behind, so a
  PSCI call traps to a vector nobody installed and the machine stops dead with nothing to report it.
  The entry exception level is now captured in `_start` (x20, alongside the DTB in x19) because it is
  the only way to know afterwards, and PSCI is skipped when we were the highest level.
- **A secondary arrives by one of two completely different routes**, and which one is not a choice:
  firmware that parks its cores releases them when the spin table is written, while QEMU delivers every
  core to `_start` immediately. Parking them at `_start` makes them unreachable by the release; both now
  funnel into one entry that waits for the page tables, so one path gets exercised instead of two that
  each only work somewhere.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/smp_boot.rs` | new, 9 | The secondary entry stub (its own exception-level ladder, FP enable, per-core stack selection and the wait for the boot core's tables), the PSCI `smc`, the spin-table write with the cache clean a core polling with its caches off requires, and the `sev` pair that wakes them. |
| `arch/aarch64/mmu.rs` | 15 -> 17 (+2) | `enable_secondary`: installs the tables the boot core already built rather than rebuilding them, because a second core writing shared tables while the first runs under them is a race with no upside. |
| `arch/aarch64/mod.rs` | 62 -> 64 (+2) | Recording and reading the entry exception level. |
| `arch/aarch64/gic.rs` | 4 -> 5 (+1) | `init_secondary`: a core's GIC CPU interface and priority mask are BANKED per core, so each must enable its own or it never receives its own timer. The distributor is shared and deliberately untouched. |

## 2026-08-04 - Pi 4: auditing the new USB code (feat/pi4-aarch64)

~2,500 lines of new ring-0 driver code that parses untrusted device input had been reviewed only
ad-hoc. Two real defects, both of the "works on the device in hand" kind:

**A silently truncated block address.** The block protocol carries a u64 LBA on purpose, and
`READ(10)`/`WRITE(10)` name only 32 bits, so `lba as u32` WRAPS. Block `0x1_0000_0000` becomes block 0
- for a write, the superblock overwritten by data meant for the far end of the disk, with every layer
reporting success. The stick in hand is 31M sectors and cannot reach it, which is exactly why it had to
be refused here rather than left for a larger device to find.

**Leaked pages on every failed enumeration.** Twelve DMA allocations, ~40 failure paths, and no
`free_frame` anywhere. Hot-plug retries an arrival three times, so a device that refuses to come up
leaked steadily as it was replugged - the unbounded-resource shape of §26.6, arrived at by omission
rather than design. The three transient pages (input context, control ring, descriptor buffer) now come
from a fixed arena allocated once and reused; enumeration is serialised under `USB_CLAIM`, so there is
never a second user. The residual - one device-context page per failed attempt, bounded by the
three-attempt cap - is recorded in the code rather than claimed fixed (§26.3).

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 35 -> 37 (+2) | The scratch arena's borrow (serialised by `USB_CLAIM`, so the only live one) and the zeroing of its three pages between devices - a ring left holding the previous device's TRBs would have the controller execute entries it has already run. |

## 2026-08-04 - Pi 4: reboot actually reboots (feat/pi4-aarch64)

`hardware_reset` was `loop { spin_loop() }`. The shell printed "rebooting...", the kernel never asked
the hardware, and the board had to be power-cycled by hand - which is indistinguishable from a reset
that failed.

Ported from the 32-bit implementation, including the part that was learned the hard way there: the boot
partition in `PM_RSTS` must be cleared. The firmware reads that field back after the watchdog fires;
left at whatever it held, the SoC resets and then sits dark, producing the *same* "reboot does nothing"
symptom one layer further on. The poke is re-issued rather than spun on, and it is bounded and loud,
because "this never returns" is an assumption about hardware.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 61 -> 62 (+1) | The BCM2711 power-management registers, reached through the kernel's Device mapping. Volatile 32-bit writes gated by the 0x5A password - the documented reset poke, and the only thing these writes can do. |

## 2026-08-04 - Pi 4: the four hot-plug bugs the hardware found (feat/pi4-aarch64)

Storage and hot-plug both worked at boot and then interacted badly. Four faults, one of them shared:

1. **Unplugging the stick froze the machine.** Enumeration was running in the TIMER TICK, and enumerating
   a device costs ~100 ms of port resets, descriptor fetches and SCSI retries - all with interrupts
   masked. It moved to the idle loop, which is interruptible.
2. **The keyboard died after a disk transfer.** One event ring serves the whole controller, so a disk
   transfer waiting for its own completion also RECEIVES the keyboard's, and discarded it. `pending`
   then stayed set forever and no further report was ever queued.
3. **A disk brought up on one port relabelled the keyboard as being on that port**, so unplugging the
   stick stood the keyboard down and unplugging the keyboard did nothing.
4. **`drives` reported 0 MiB** while the kernel had found a 15 GB stick: `block-driver` was never
   granted `USB_DISK` on aarch64, so the capacity syscall refused with `CapNotHeld`.

Exclusion is now one claim (`USB_CLAIM`) across all three drivers of this hardware - tick, syscall,
idle - rather than a one-way flag, and holders run with interrupts enabled while everyone else stands
aside. The disk answers BUSY, which the block driver already retries.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 34 -> 35 (+1) | `hotplug_poll` borrowing the controller from the idle loop, under the claim that makes it the only context touching it. |

## 2026-08-04 - Pi 4: USB mass storage (feat/pi4-aarch64)

Storage on the Pi 4 is the **USB stick, never the SD card**. The board has one SD slot and boots from
it - firmware, kernel image, FAT partition - and GSFS puts its superblock at LBA 0, exactly where that
card's partition table is. The 32-bit port established this by corrupting two boot cards to RAW before
its SD backend was withdrawn; the Pi 4 has the same topology and inherits the rule rather than
rediscovering it.

Bulk-Only Transport over the two bulk endpoints, with SCSI `TEST UNIT READY` / `READ CAPACITY(10)` /
`READ(10)` / `WRITE(10)` / `SYNCHRONIZE CACHE(10)`. Above the driver nothing changed: the existing
`arch::imp::usb_disk_*` seam, `usbdisk.rs` and `fs` are all arch-neutral and were already waiting for a
backend.

The one genuinely new hazard is that the disk is driven from a **syscall** while the keyboard is driven
from the **timer tick**, and both consume the same event ring. An ISR that pops the disk's completion
leaves the block driver waiting forever for an event that has been thrown away. `DISK_IO` makes the ISR
yield for the duration - it costs one keyboard sample. A future SMP port needs a real lock there, and
the comment says so.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 26 -> 34 (+8) | Building the 31-byte Command Block Wrapper and reading the 13-byte Command Status Wrapper back (the CSW is placed 64 bytes clear of the CBW so a device that writes status early cannot land it on the command still being read); the `READ CAPACITY` reply; the block copies in and out of the DMA page, both length-checked against 512 by their callers; the input context for a two-endpoint Configure Endpoint; and the borrow of the controller in `with_disk`. |

## 2026-08-04 - Pi 4: USB hot-plug (feat/pi4-aarch64)

Everything on this board is behind the internal hub, so a driver that enumerates once at boot leaves a
keyboard unplugged mid-session dead until reboot - and, worse, means the keyboard only ever works if the
cable happened to be in at power-on. The watcher visits the hub's ports once a second and announces every
transition, the same way the x86 and Pi 2 ports do.

Two bounds it is built around. It does **not act on a change it could not acknowledge**: an event whose
latch will not clear is re-reported on every visit, which would tear down a working device once a second
forever. And an arrival gets **three attempts**, not unbounded retries (§26.6) - a device that will not
enumerate is left alone until the port changes again. Standing a device down also **releases its slot**,
because a slot is a bounded resource and a leak would run the controller out after thirty cable pulls.

The watcher runs inside the timer tick with interrupts masked, so its transfers use a short deadline
(50 ms) while enumeration keeps the generous one (500 ms). A transfer that hangs there does not merely
delay the watcher; it stops the scheduler.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 24 -> 26 (+2) | Reading the port status and change words in the watcher, and the keyboard-state static it disarms when a device is stood down. Both are the module's own memory, read immediately after the transfer that filled it. |

## 2026-08-04 - Pi 4: hub enumeration (feat/pi4-aarch64)

The Pi 4's USB-A sockets do not hang off the VL805's root ports. They hang off an **internal VIA hub**,
which the root port reports as a single high-speed device - so a keyboard plugged into the machine is
one tier further out than the driver could see, and the bring-up ended at "device is not a HID boot
keyboard" while pointing at a hub.

Walking a hub adds three things the root-port path never needed, and leaving any of them out produces
an `Address Device` failure whose completion code points somewhere else: the **route string** (which
downstream port to take at each tier), the **transaction translator** that a high-speed hub provides so
a low or full speed device can be reached across a fast link, and the fact that the device's speed comes
from the **hub's** port status rather than from `PORTSC`, which sees only the hub.

Only one tier is walked. That covers this board and a keyboard plugged into it; a hub plugged into a hub
says so rather than recursing into an unbounded walk of someone's docking station.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | 18 -> 24 (+6) | Reading the hub descriptor's port count, the 4-byte port-status replies (one per port, and again while polling a reset), the configuration descriptor's `bConfigurationValue`, and the device descriptor slice that identifies what was found. Every one is a read of the module's own descriptor buffer immediately after a control transfer filled and synced it; the two that index it do so at fixed offsets inside a 16-byte or 18-byte reply whose length was checked. |

## 2026-08-04 - Pi 4 milestone 23: PCIe and the USB keyboard (feat/pi4-aarch64)

The Pi 2's USB host controller sits on the SoC's peripheral bus at a fixed address. The Pi 4's does
not: its USB-A ports are behind a **VIA VL805**, an off-SoC xHCI controller on the far side of a PCIe
Gen2 link. Before one USB register can be read, the root complex has to leave reset, the link has to
train, an address window has to be opened onto the bus, config space reached through it, and a BAR
assigned inside it. `pcie.rs` does that and stops - it knows nothing about USB. `xhci.rs` starts at the
BAR and ends where `crate::arch::hid` begins.

**A probe read that tolerates an external abort (`uaccess::probe_read32`) is what keeps QEMU usable.**
The root complex is at a fixed address on a real Pi 4 and at *nothing* under `raspi4b`, and the
difference does not present as a translation fault - the mapping is perfectly valid - but as an
external abort from the interconnect, which at EL1 is indistinguishable from a kernel bug and halts the
machine. That is the right default and the wrong answer for a probe, whose entire question is "is
anything there?". It reuses the user-copy seam's fixup for exactly one load, so a fault anywhere else
still halts loudly.

**The driver runs in ring 0 and this is a TCB expansion**, the same one §6.4's 2026-07-23 amendment
already records for ARM: neither ARM port routes device IRQs to userspace, so an in-kernel driver parses
untrusted device-supplied descriptors, on a board with no IOMMU to confine its DMA. Recorded rather than
papered over (§26.3). Every descriptor walk here is bounded by the length byte and refuses a zero-length
entry, because a malformed descriptor from an untrusted device must end the walk, not spin it forever.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/xhci.rs` | new, 18 | Ring and context construction, and the report buffer. Every one is a write to a page this module allocated from the frame allocator and named through the direct map, whose layout is the xHCI 1.2 structure the controller reads. The two `read_volatile`/`write_volatile` register helpers are the controller's BAR, which `pcie` mapped and assigned. `parse_config` builds a slice over the descriptor buffer so the walk itself is bounds-checked. The scratchpad pointer-array writes are bounded by an explicit refusal above 512 entries, so a controller asking for more than one page can index is turned away rather than allowed to overrun it. |
| `arch/aarch64/pcie.rs` | new, 4 | The root-complex register read and write helpers (volatile accesses inside the kernel's Device mapping), the presence probe, and the probe read of the first dword through the outbound window - which distinguishes "the CPU address is not decoded to PCIe" (abort) from "it reaches the root complex, which will not forward it" (poison) from "it arrived". Those want three different fixes, and guessing between them cost two hardware iterations. |
| `arch/aarch64/uaccess.rs` | 5 -> 7 (+2) | `probe_read32`: the fixup pointer, and the one-instruction `asm!` block that arms it, does the load, and clears it on both exit paths. |
| `arch/aarch64/mmu.rs` | 12 -> 15 (+3) | `dma_sync` (`dc civac` over a range shared with a bus master - the BCM2711's PCIe is not I/O-coherent, SEC-28), and the two `fill_device_window` writes that build the sparse PCIe outbound-window table. |
| `arch/aarch64/mod.rs` | 59 -> 61 (+2) | The write and the read of `PCIE_XHCI`, the static carrying the discovered controller from the boot probe to the driver. |
| `arch/aarch64/exceptions.rs` | 11 -> 12 (+1) | `end_probe`: `dsb sy` to complete outstanding accesses, then briefly unmask `PSTATE.A` so a pending SError is delivered INSIDE the probe window rather than at the next entry to EL0 - which is where the first bring-up's abort actually surfaced, blaming an EL0 selftest 180 ms and one subsystem away from the write that caused it. |
| `arch/aarch64/uart_rx.rs` | 2 -> 3 (+1) | `push`: a keyboard delivering a byte into the ring the console reader already drains. The ring is shared with serial deliberately - the console has one input stream, and a blocked reader should not have to know which device a byte came from. |

## 2026-08-04 - Pi 4: the page-table primitives the chaos run needed (feat/pi4-aarch64)

Four `page_tables` primitives were still stubs, and the difference between them mattered. Three were
*quiet* stubs (`read_page_table_base` returning 0, `write_page_table_base` and `invalidate_tlb_page`
doing nothing); one, `map_in_active_tables`, was an `unimplemented!()` - which is not a stub that fails
at its caller, it is a **kernel panic**. The first thing to reach it was a chaos run's memory pressure:
a service asking for heap took the whole machine down. Nothing had reached it in twenty-one milestones
because no service had grown its heap.

`map_in_active_tables` maps into whatever `TTBR0_EL1` holds, which is the *running* task's table. That
cannot go through a `PageTable`, whose `Drop` frees the tree - wrapping the live root in one and letting
it fall out of scope would free the address space of the task currently executing. `ptables::map_in_root`
says the intended thing instead: borrow the root, do not own it.

`write_page_table_base` deliberately performs **no** TLB maintenance, and the reason is recorded at the
primitive rather than left to be rediscovered (SEC-27, `arch/CLAUDE.md`: every `arch::imp` primitive owes
a documented semantic, not just a signature). On AArch64 a `TTBR0_EL1` write flushes nothing, so an
address-space *change* must invalidate - and `context::switch_context` does, which is the only path that
switches between two task spaces. This primitive exists for the shootdown path, which rewrites the value
already installed; a flush here would be a second, redundant one on every context switch.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 55 -> 59 (+4) | `read_page_table_base` (an `mrs`, side-effect-free); `write_page_table_base` (`msr ttbr0_el1` + `isb`, so the next access is translated by the new table); `invalidate_tlb_page` (`dsb ishst` / `tlbi vaae1is` / `dsb ish` / `isb` - by VA, all ASIDs, inner-shareable, because the neutral shootdown path needs it visible machine-wide); and `map_in_active_tables` forwarding to `ptables::map_in_root`. |
| `arch/aarch64/ptables.rs` | 19 -> 20 (+1) | `map_in_root`: maps one page into a live root the caller does not own, via `ManuallyDrop` so the borrow cannot free the running task's address space. Uses the same `map` path already audited. |


## 2026-08-04 - Pi 4: real services + the diagnostics that found the next bug (feat/pi4-aarch64)

`ping` and `pong` are now built and embedded for aarch64, and spawned by name through
`spawn_service_by_name` - the same path the supervisor's spawn syscall takes.

**Two real bugs, one in the port and one still open:**

`pong` would not spawn at all: `PlacementInvalid`. It contracts core 1 and this port has one core, so it
needs an explicit override (§14.4) - but the override checks `smp::core::is_ready(0)`, and **this port
never told the neutral SMP layer that core 0 exists**. Nothing else noticed, because every other
placement path falls back with `.max(1)` and lands on core 0 regardless; the STRICT contracted-placement
rule is the one path that asks properly, and it correctly refused. `mark_ready(0)` at boot.

Still open: `pong` faults at EL0 inside `memcpy`, storing 8 bytes at exactly `0x8000_0000` - one word
past `USER_STACK_TOP`. The top stack page IS mapped (`[0x7FFFF000, 0x80000000)`), so this is a genuine
overrun rather than a mapping gap.

**The trap report gained `SP_EL0` and the faulting task's NAME**, and both earned their place
immediately. Without the name I spent several rounds disassembling `ping` - the wrong binary - because I
had assumed which service faulted. A user-mode fault reported without the task's name makes the reader
guess, and a guess about which service faulted is the wrong place to start.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | 10 -> 11 (+1) | Reading `SP_EL0` in the trap report - a side-effect-free system-register read. A userspace fault is very often the stack rather than the code, and `FAR` alone cannot distinguish "wrote past its frame" from "SP was never set correctly". |

## 2026-08-04 - Pi 4 milestone 19: serial input (feat/pi4-aarch64)

Nothing on this port had read input before; output has been one-way since milestone 1, which is why
there is still no prompt. The PL011 receive FIFO now drains into a ring the neutral `ConsoleRead` path
pops from, driven both by the timer tick and directly by a blocked reader (a starved ISR would
otherwise leave a byte stranded in the FIFO with a reader asleep waiting for it).

**The 32-bit port's hard-won lesson is inherited rather than rediscovered:** the PL011 reports receive
errors in the SAME read as the data, in `DR` bits 11:8. Masking them off - the obvious thing to write -
silently promotes line noise to input. On the Pi 2 a GPIO HAT held RX low, which the PL011 reports as a
continuous break; each one enqueued a spurious `0x00`, the ring filled with nulls, and a full-screen
editor blocked on `ConsoleRead` repainted 966 times while the document changed twice. Flagged bytes are
discarded here from the outset.

A persistently overflowing line is switched off (`RXE` cleared, output untouched, latching until
reboot) after a **duration** of unbroken overflow rather than a byte count - a count is not a measure of
how long a condition has persisted, and choosing one is how you get a threshold that either fires on a
fast typist or never fires at all.

Proven by feeding characters in: `gsh test 123` echoed back, 12 of 12, in order, with the logger service
still running alongside.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/uart_rx.rs` | new, 2 | The bounded FIFO drain (volatile PL011 reads plus the error-clear write) and the ring `pop`, whose slot the producer published with a Release store. |

## 2026-08-04 - Pi 4: the SAME cache bug, in the loader this time (feat/pi4-aarch64)

The fix below was applied to the two hand-written payload copies and **not to the ELF loader** - even
though its own commit message said the loader would need it. Fixing the instances rather than the class
left it open where it mattered most.

On hardware, the logger's text landed in frames the one-shot EL0 demo had just executed from and freed.
The I-cache still held the old blob, so the "logger" ran the DEMO's code, hit the demo's exit syscall,
switched to a `CTX_KERNEL` saved during boot, and resumed execution in the middle of `boot_high` - which
re-ran the rest of the boot. It looped until the endpoint table filled, 95 task slots later.

Two fixes, because the bug had a cause and an amplifier:

- **Cause:** `finalize_service_address_space` - the arch hook the neutral loader already calls for every
  service - now syncs the I-cache over every page of the new address space. Applying it at the hook
  covers every service the loader will ever produce, rather than the ones someone remembered.
- **Amplifier:** `CTX_KERNEL` was a resurrection point. The exit syscall now refuses when the demo is
  not running, so the same class of mistake costs one loud refusal instead of a boot loop.

The hook reports how many pages it synced, because QEMU models no separate I-cache and cannot
demonstrate the fix - the only evidence available in emulation is that the hook ran and did work. A
silent hook is exactly how this shipped once already.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/ptables.rs` | 19 -> 21 (+2) | `sync_all_pages` walks the new address space and syncs each leaf through the kernel's direct map (cache maintenance is by physical line, so the task's own VAs need not be mapped). |
| `arch/aarch64/mod.rs` | 51 -> 53 (+2) | `finalize_service_address_space` calling it, and the count it reports. |
| `arch/aarch64/usermode.rs` | 14 -> 16 (+2) | The `DEMO_ACTIVE` guard around the exit switch, and setting/clearing it around the demo. |

## 2026-08-04 - Pi 4: writing code needs cache maintenance (feat/pi4-aarch64)

Found on hardware, on the first boot of milestone 17, and **structurally invisible under emulation**.

The instruction and data caches are not coherent on ARM. A store lands in the D-cache while the I-cache
may still hold whatever was previously at that physical address. The kernel writes code every time it
loads a program, so this is not a demo concern - the ELF loader will hit it the moment it loads a
service's text.

What happened: a task payload was copied into a frame the previous EL0 task had just executed from and
freed. The allocator handed the same frame straight back, the I-cache still held the old blob, and the
core executed the **previous task's instructions** until it ran off the end into an Illegal Execution
State (`ESR` EC `0b001110`, `PSTATE.IL` set). The give-away in the log was the scheduled task printing
the one-shot demo's messages. QEMU models no separate I-cache, so it had passed there a dozen times.

`sync_instruction_cache` cleans the range out of the D-cache to the point of unification and
invalidates the I-cache over it. Line sizes come from `CTR_EL0` rather than being assumed - `DminLine`
and `IminLine` differ between implementations, and a hardcoded 64 either does needless work or, if too
large, skips lines.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | 11 -> 12 (+1) | `sync_instruction_cache`: reads `CTR_EL0` for the line sizes, then `dc cvau` / `ic ivau` over the range with the `dsb`/`isb` that order them against the fetch which follows. Cache maintenance by VA alters no architectural state beyond the caches. |

## 2026-08-03 - Pi 4 milestone 17: an EL0 task under the neutral scheduler (feat/pi4-aarch64)

Every earlier EL0 excursion was a one-shot: the boot dropped to EL0, the task ran, control came back.
This is an EL0 task the **scheduler** enters, the timer preempts, and other tasks share a core with.

`TaskContext::new_user` bridges a real mismatch: `switch_context` ends in `ret` and stays at EL1, while
entering EL0 needs an `eret`. So the context points at a trampoline that performs the `eret`, carrying
the user entry and stack in `x19`/`x20`. x86 solves the same mismatch the same way, differing only in
where the values ride. The context's `sp` is the task's KERNEL stack, and that is load-bearing beyond
first entry: after the `eret` it stays in `SP_EL1`, so it is the stack every later trap from this task
lands on.

**The bug this found had been sitting there for four milestones.** `syscall_slot` returned
`core::ptr::null_mut()`, and `prepare_ring3_switch` - which runs *only* for tasks marked `is_user` -
writes through it. Nothing had ever been marked user, so the stub was unreachable and invisible; the
first scheduled EL0 task turned it into a null write on the context-switch path. A stub reachable down
exactly one path stays silent until something takes that path, which is why the crash arrived a whole
milestone after the stub was written. It is now a fixed array (not the per-core arena, for the same
reason `uaccess` uses one: the context-switch path must not depend on an allocation having happened),
with a comment saying plainly that AArch64 does not consult these fields - the hardware selects
`SP_EL1` architecturally - but the neutral scheduler maintains them, so they need real storage.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/sched_user.rs` | new, 4 | Copying the position-independent EL0 payload into its frame, and building + committing the task (its kernel stack is a module-owned static; the slot is freshly reserved; the user pages are mapped with EL0 access in the table the switch installs). |
| `arch/aarch64/mod.rs` | 50 -> 51 (+1) | `syscall_slot` handing out a pointer into its fixed per-core array. |

## 2026-08-03 - Pi 4 milestone 16: real syscall dispatch (feat/pi4-aarch64)

`svc` now reaches the **neutral** `syscall::dispatch::syscall_handler` - the same function x86 and
arm32 call, with the same numbering. Bring-up numbers live above every real one (`>= 0x1000`) so the
two ranges cannot collide while both exist, and the whole bring-up range disappears with the demo.

The neutral subsystems (per-core arenas, scheduler slots, capability table) moved into the main boot
rather than only the scheduler demo, because a real syscall reaches `current_task_lookup_cap`, which
indexes per-core state - and reaching that before it exists is exactly how the user-copy seam went
silent in milestone 15.

What the EL0 task now proves, from real userspace through the real ABI:

- **`Log` with no capability is REFUSED** (`-2`, `CapNotHeld`). §3.1 enforced end to end on AArch64:
  authority comes from holding a capability, never from being the caller.
- **An unknown syscall number returns a defined error** (`-1`), not a fault (§22 Fuzz F2).

The unknown-number test needed fixing after it appeared to pass: `0xBEEF` is *above* the bring-up base,
so the call went to the demo handler and proved nothing about the neutral path. `0x0FFF` reaches the
dispatcher. The log's `WARN unknown svc #48879` is what gave it away.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | 9 -> 10 (+1) | Calling the neutral `syscall_handler` - the ring-3 to ring-0 transition it is written for; it treats every argument as untrusted and validates pointers through the user-copy seam. |
| `arch/aarch64/usermode.rs` | 13 -> 14 (+1) | Recording the dispatcher's verdicts in a module-owned static so `run` can check both. |

## 2026-08-03 - Pi 4 milestone 15: the user-pointer copy seam (feat/pi4-aarch64)

Where the kernel touches memory a task chose the address of. Three defences, because each catches what
the others cannot:

1. **A range check** - pointer and length inside the user half, addition checked for overflow.
2. **`ldtrb` / `sttrb`, not `ldrb` / `strb`** - the UNPRIVILEGED load and store. Executed at EL1 they
   apply **EL0 permissions**, so the hardware refuses a kernel address even if the range check were
   wrong. Defence in depth at no cost: same instruction count, same speed, and a bug in check (1) stops
   being exploitable.
3. **A fault fixup** - a range-valid pointer can still be unmapped, and the abort lands at EL1 looking
   exactly like a kernel bug. Unguarded, that halts the machine: a denial of service any service could
   trigger by passing a bad pointer. Vector 4 (same-EL synchronous) became recoverable, and the copy
   helpers register a recovery address around *only* the faulting instruction - so a kernel bug faulting
   anywhere else still halts loudly, which is what makes this safe rather than a blanket "ignore kernel
   faults".

Reads are **copied** into a per-core staging buffer rather than borrowed: handing the kernel a pointer
into user memory leaves every later read racing the task, which can change the bytes between validation
and use.

**The bug this cost is the one worth keeping.** The per-core state was first indexed via
`current_core_id()`, which needs tables that are not up when the first copy runs - and, far worse, the
FAULT HANDLER calls into this module. Indexing an unallocated arena from a fault handler faults again,
and the second fault happens while reporting the first, so the machine went completely silent with
nothing printed. A fault handler must not depend on initialisation order. It now indexes a fixed array
by `MPIDR_EL1.Aff0`, which needs no setup and cannot fail.

The selftest drives all four outcomes including the two that only fire on bad input, and counts
recovered faults so "the unmapped pointer survived" is backed by evidence the fixup FIRED rather than by
something upstream having quietly rejected the pointer.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/uaccess.rs` | new, 5 | `core_index` (an `MPIDR_EL1` read), the two `asm!` copy loops (unprivileged accesses plus fixup arm/disarm), the staging-buffer pointer, and the slice built over the copied bytes. |
| `arch/aarch64/exceptions.rs` | 7 -> 9 (+2) | `aarch64_sync_current_dispatch`: reading `ESR_EL1` and rewriting the trap frame's `elr` to the recovery address. |
| `arch/aarch64/usermode.rs` | 11 -> 13 (+2) | Installing the task's `TTBR0` around the seam selftest, so it runs against a REAL user page - the only way to test checks that are all about an address the kernel does not own. |

## 2026-08-03 - Pi 4 milestone 14: `ptables` becomes the real `page_tables::PageTable` (feat/pi4-aarch64)

`loader.rs` calls `arch::imp::page_tables::PageTable::new()`, which was still the `unimplemented!()`
stub - so no service ELF could be loaded no matter what else worked. The hardware-proven implementation
is now wired in behind the neutral signature (`map(VirtAddr, PhysAddr, PageFlags)`), and the arch-native
form is kept as `map_raw` for the EL0 bring-up path.

The flag translation has one decision worth naming: **`PCD | PWT`**. On x86 those disable caching for an
MMIO mapping; the faithful AArch64 equivalent is not "uncached Normal" but the **Device** attribute,
which additionally forbids the reordering, merging and speculative repetition that make a wrongly-typed
MMIO mapping misbehave in ways no fault points at.

Two hooks that were empty stubs now have bodies, and one is deliberately still empty:
`free_page_table_root` frees the table tree; `reclaim_user_frames` frees the leaf pages (an empty stub
there would not fail loudly - it would leak every page of every task that ever died, surfacing only as
the machine slowly running out of memory); and `finalize_service_address_space` is genuinely a no-op,
because unlike the 32-bit port there is no kernel map to clone into a new space.

Reclaim is **proven, not assumed**: the selftest now maps extra pages, hands the space to
`reclaim_user_frames`, and checks both the count returned and the allocator's free total - `4 pages
reclaimed, all frames returned`.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 45 -> 50 (+5) | `free_page_table_root` and `reclaim_user_frames` forwarding to `ptables`, plus the selftest's reclaim exercise (mapping extra pages and calling reclaim on the space). |
| `arch/aarch64/ptables.rs` | 19 (net 0) | `reclaim_pages` walks the tree and frees each leaf, using the same `get`/`set` accessors already audited; the kernel-copy loop it replaces was the same count. |

## 2026-08-03 - Pi 4 milestone 12: an EL0 task in its own address space (feat/pi4-aarch64)

The payoff of the TTBR1 split, and the first place the separation is enforced by hardware rather than
by leaving the right bits clear. `PageTable::new` now allocates **one** frame and copies nothing: a task
table holds the task and nothing else, because the kernel is in `TTBR1` where no `TTBR0` switch can
disturb it and no EL0 access can reach it. That deleted the 20 KiB kernel-copy per address space AND
the collision class it carried.

The task is frames, not linker sections: a code page and a stack page from the allocator, with a
position-independent payload copied in. Position-independent because it is linked into the kernel at a
high address and executed at a low one - trivially so, being register operations and `svc` with no
memory reference.

Net unsafe is unchanged: what `usermode.rs` gained (building the space, copying the payload, installing
`TTBR0` before the `eret`) is offset by what `ptables.rs` lost with the kernel-copy loop.

**Page 0 is now reserved.** The allocator handed out physical frame 0 as a page-table root, which
printed `TTBR0=0x0` - working, but by coincidence, and colliding with the `cr3 == 0` sentinel
`switch_context` uses for "no address space". A null physical address must never be a valid allocation.

## 2026-08-03 - Pi 4 milestone 11b: the kernel moves to the high half (feat/pi4-aarch64)

The kernel is now LINKED high and LOADED low (VMA `KERNEL_VA + 0x80000`, LMA `0x80000`), relocates
itself into `TTBR1` early in boot, and then **retires the low identity map**. `TTBR0_EL1` is empty from
that point on, which removes the collision milestone 10 documented: a task page below 4 GiB can no
longer shadow the kernel, because the kernel is not there.

Three things make the transition safe, and they are all in the code rather than in anyone's head:

- `_start` reaches symbols only through `adrp`/`add`, which is purely PC-relative, so the same
  instruction yields the right LOW address while the MMU is off and the right HIGH one afterwards.
- Between `enable` and the jump, BOTH halves translate. There is no instant at which the core executes
  from an address that does not resolve.
- Peripherals move first (`mmio_go_high`), because a device register is still named by its physical
  address and the UART must survive the step or the next failure reports nothing.

**Two mistakes here are worth keeping.** The relocation asm used a hardcoded `x9` as scratch while
letting the compiler allocate `{base}` - and it chose `x9` too, so `mov x9, sp` destroyed the base and
`orr x9, x9, x9` left SP unchanged. The kernel then ran on happily from its still-mapped low stack and
only died when the low map was retired, whereupon the handler faulted on its own push and recursed,
walking `FAR` down by one 272-byte frame per iteration. Nothing pointed at the asm; reading SP back and
printing it settled it in one run, and the boot now reports SP permanently for that reason. Separately,
the high-half selftest initially compared an address against itself (it took the symbol's address as
the "high" side, but pre-jump codegen yields a LOW address) - it passed while testing nothing.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | 5 -> 11 (+6) | `virt_to_phys` (linker symbols are high; anything the HARDWARE consumes must be physical); the high-half table build; `jump_high` (reads SP, then relocates SP and PC together); `running_high` (reads the PC rather than assuming); `drop_low_map` (`msr ttbr0_el1, xzr` + invalidate); and the corrected selftest. |
| `arch/aarch64/mod.rs` | 42 -> 45 (+3) | The `mmio()` indirection now used by every PL011/GPIO access, and the SP read-back that reports the stack relocated. |
| `arch/aarch64/memmap.rs` | 7 -> 8 (+1) | `current_map`, which re-derives the map after the jump abandons the low stack along with every local on it. |

## 2026-08-03 - Pi 4 milestone 11a: the TTBR1 high half goes live (feat/pi4-aarch64)

Stage one of moving the kernel out of `TTBR0`. The high half (`TTBR1_EL1`) now maps physical `P` at
`KERNEL_VA_BASE + P` - a direct map, the same thing Limine's HHDM is on x86 - alongside the existing
low identity map. Nothing has moved yet; this proves the mapping works before anything is built on it.

`KERNEL_VA_BASE` is chosen so `>> 30 & 511 == 0`, which makes the high L1's entry *i* cover the same
GiB as the low L1's entry *i*, so both maps are built by the same loop rather than by two subtly
different ones.

**`TG1` does not share `TG0`'s encoding.** TG0 spells 4 KiB as `0b00`; TG1 spells it `0b10`. Copying
the TG0 value across selects a 16 KiB granule, every high walk fails, and the fault points at whatever
first touched a high address rather than at the register. The selftest exists to catch exactly that
class at the point of the mistake.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | 4 -> 5 (+1) | `selftest_high_half` writes through the physical (identity) address and reads back through the high alias, then reverses the direction so it cannot pass on a stale line that happened to match. The high tables themselves are built inside the existing `enable` block, so they add no new `unsafe`. |
| `arch/aarch64/mod.rs` | 41 -> 42 (+1) | Freeing the scratch frame the selftest borrowed from the allocator. |

## 2026-08-03 - Pi 4 milestone 10: per-task page tables (feat/pi4-aarch64)

An address space is a private L1 plus **its own copies** of the four kernel L2 tables. The alternative
- pointing each task's L1 at the shared kernel L2s, as the 32-bit port does - saves 20 KiB and buys an
aliasing hazard: a table split in one address space is seen by all of them, and reclaim then has to
distinguish a table the task owns from one it merely points at. Copying means **nothing is aliased**,
so reclaim frees everything the root reaches without a single ownership test.

**This milestone also surfaced a real architectural limit, and the code says so rather than working
around it.** A task page below 4 GiB would land inside a 2 MiB block of the kernel's identity map, and
mapping it would shadow the kernel's own view of that physical range - `USER_STACK_TOP` (0x8000_0000)
sits directly above the frame allocator's bitmap. `map` refuses that case loudly instead of splitting
the block, because the honest fix is to stop putting the kernel in TTBR0 at all: the kernel belongs in
TTBR1 (high VA), leaving TTBR0 entirely to the task. The speculative block-splitting code that would
have papered over it was written and then **deleted** (§26.2 - features are pulled into existence, not
kept in case).

The selftest exercises the `TTBR0_EL1` swap that milestone 9 shipped unexercised: build a space, map a
page above the kernel map, install the new TTBR0, read the value back through the new mapping, confirm
kernel memory is still reachable under the task's table, switch back, and check every frame returned.
Proven to fail by inverting the value comparison (`mapped=false`, every other counter clean), and the
`map failed` path fired on its own during development.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/ptables.rs` | new, 19 | Page-table walking and construction: `get`/`set` on identity-mapped table frames, zeroing a freshly allocated table (garbage would have VALID bits set at random and the walker follows them), copying the kernel L2s, and `free_all` reclaiming the tree. Every one is a raw access to a frame this module allocated or to the kernel's own boot L1. |
| `arch/aarch64/mmu.rs` | 3 -> 4 (+1) | `kernel_l1_root` takes the address of this module's static L1 - no dereference. Per-task spaces copy from THIS, never from the live `TTBR0_EL1`, which belongs to whichever task is running; the 32-bit port copied the live root and gave a child its spawner's user entries, which the child's reclaim later freed out from under the still-running spawner. |
| `arch/aarch64/mod.rs` | 33 -> 41 (+8) | The page-table selftest: writing and reading the test frame and the kernel canary through identity addresses, reading `TTBR0_EL1`, the two `msr ttbr0_el1` + `tlbi vmalle1` switches, and freeing the two frames. The switch is safe because the new space maps the kernel (copied from the boot L1), so the instruction stream, stack and UART stay translated across it. |

## 2026-08-03 - Pi 4 milestone 9: the neutral scheduler preempts (feat/pi4-aarch64)

Three kernel tasks that deliberately never yield, round-robined by the neutral `scheduler::run` under
the 100 Hz generic timer. The scheduling decision is the same neutral code x86 runs; what this commit
adds is the arch side it stands on.

**The trampoline is the load-bearing piece.** The scheduler masks IRQs before its initial
`switch_context`, so a task whose `lr` pointed straight at its entry begins running with `DAIF.I` set
and - never yielding - never has it cleared. Observed exactly: task A ran correctly and forever while
B and C starved, which reads as a broken scheduler when in fact it was never given a tick. x86 solves
it the same way (`task_entry_trampoline` does `sti` then `ret`); this port carries the entry in x19
rather than on the stack, because `ret` here jumps to `lr` rather than popping.

**The GIC EOI had to move before the switch, not after.** The neutral tick performs a preemptive
`switch_context` internally and may not return on this task's stack at all. The GIC CPU interface keeps
a priority active until the interrupt is retired, so an EOI deferred past the switch leaves the timer
interrupt permanently active and blocks every later interrupt of equal or lower priority - one tick,
then silence, with the scheduler looking like the culprit.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/context.rs` | 2 -> 9 (+7) | The `TTBR0_EL1` read/write/invalidate in `switch_context` (SEC-26: writing TTBR0 flushes nothing on AArch64, so the switch must invalidate on an address-space change - route (a) from `arch/CLAUDE.md`); the entry-trampoline `global_asm!` and its extern; and `new_user`, which is a loud `unimplemented!` rather than a context that would `ret` into a user address at EL1. Note the TTBR0 branch is NOT yet exercised - every task shares the kernel identity map - and that is stated in the code rather than assumed. |
| `arch/aarch64/mod.rs` | 28 -> 33 (+5) | Real bodies for primitives the neutral scheduler depends on, previously no-op stubs: `enable`/`disable_interrupts` (`msr daifclr/daifset, #2` - note DAIF is a MASK, so the polarity is inverted from x86's `IF`), `local_irq_save` (reads DAIF), `wait_for_interrupt` (`wfi`), and `read_cycle_counter` (`CNTPCT_EL0`, with an `isb` first so the read is not speculated earlier and a measured duration does not come out short). |
| `arch/aarch64/sched_demo.rs` | new, 5 | Demo-only: three 64 KiB `.bss` stacks this module owns, built into task contexts and committed to freshly reserved scheduler slots during single-threaded boot. |

## 2026-08-03 - Pi 4 milestone 8: the neutral frame allocator (feat/pi4-aarch64)

The first arch-neutral kernel code to run on this board. `crate::memory::init` is the same code the
x86 build has used since v1, reached through the `BootInfo` the seam defines - the demarcation claim
tested on a second ISA in the place it is easiest to get wrong.

The `unsafe` added is a **selftest**, not plumbing. `memory::init` printing a free count shows the map
parsed; it does not show that a returned frame is real, distinct, writable, or reusable. The selftest
writes every frame with a value derived from its own physical address, then verifies every frame in a
SECOND pass. The two passes are the point: write-then-read each frame in turn cannot detect ALIASING,
because two distinct physical addresses backed by the same RAM would each read back correctly the
instant after being written. A memory map claiming RAM the board does not have shows up as address
wrap or aliasing on real silicon far more often than as a fault, so this is the failure the port is
actually exposed to. Proven to fire by corrupting one frame: `1 bad read-back`, correct index counted,
every other counter clean.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 27 -> 28 (+1) | The allocator selftest's frame writes and read-backs, plus the `free_frame` calls that return them. The frames come from `alloc_frame` and the low 4 GiB is identity mapped as Normal writable memory, so a physical address is a valid pointer and nothing else holds these frames. Also in this commit, at no cost to the count: `serial_write_byte` now routes through `put_byte` (which polls TXFF) instead of writing the data register directly - it previously dropped bytes the moment the 32-entry FIFO filled, which nothing had noticed because nothing called it until neutral code began logging. |

## 2026-08-03 - Pi 4 milestone 7: the memory map (feat/pi4-aarch64)

The board does not tell the ARM how much RAM it has unless asked. Two sources are read, both before the
MMU and caches come on: the **device tree** the firmware passes in `x0` (authoritative), and the
**mailbox** `GET ARM MEMORY` tag (a fallback that cannot describe RAM above 4 GiB and, on the usual
firmware configuration, under-reports a board with more than 1 GiB).

Both are **untrusted firmware input**, and the `unsafe` here is dominated by that fact rather than by
the hardware access. Every device-tree offset is bounds-checked against the header's own `totalsize`
before it is read, the structure walk is iteration-bounded, and anything that fails to parse yields
`None` rather than a partially-believed map. That asymmetry is deliberate: a map that is wrong in the
safe direction costs capacity, while one that is wrong in the unsafe direction hands the allocator RAM
that does not exist and surfaces much later as corruption.

`_start` also gained one instruction, and it closes a real hole: the firmware hands over the device-tree
pointer in `x0`, and the first instruction of the EL2 check clobbered `x0`. The pointer was being thrown
away before anything could read it. It is now stashed in `x19` (which survives the `eret`) and stored
after `.bss` is zeroed.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mailbox.rs` | new, 3 | (1) The mailbox handshake: volatile reads of `MBOX_STATUS`/`MBOX_READ` and a write to `MBOX_WRITE`, all identity-mapped Device memory, both waits bounded so a silent GPU cannot hang the boot (invariant 12). (2)+(3) Staging the request in a module-owned 16-byte-aligned static and reading the reply back, during single-threaded boot with caches off - which is the point: the GPU reads the buffer out of RAM directly, so running before `mmu::enable` removes the cache-maintenance question rather than answering it. |
| `arch/aarch64/memmap.rs` | new, 7 | (1) Reading the linker's `__kernel_end` - taking a symbol's address does not dereference it. (2)-(4) The three bounds-checked device-tree accessors (`u32_at`, `u8_at`, and the header/magic read in `open`); each refuses any offset outside `totalsize` BEFORE reading, so a malformed blob produces `None` rather than a wild read. (5)+(6) Building the region array in a module-owned `.bss` static during single-threaded boot. (7) Handing that array out as a `&'static [MemoryRegion]` - the storage is static and the slice is read-only and never rebuilt. |
| `arch/aarch64/mod.rs` | 27 -> 27 (net 0) | The `_start` change is inside the existing `naked_asm!` block, so the count is unchanged: `mov x19, x0` before the EL2 check preserves the device-tree pointer, and `mov x0, x19` hands it to Rust. |

## 2026-08-03 - Pi 4: EL0 gets its own 2 MiB region (feat/pi4-aarch64)

Follow-up to milestone 6, after two hardware failures traced to one cause.

**In the EL1&0 translation regime, a region accessible from EL0 is forced PXN** - the kernel may not
execute what userspace can reach (ARM's equivalent of x86 SMEP). Granting EL0 access to the block
holding the kernel's `.text` therefore made the kernel non-executable at EL1, and the core died on its
next instruction fetch with no way to report it: the exception handler could not fetch its vector
either. It presented first as `tlbi vmalle1` hanging and then as the MMU enable hanging - the same
permission fault landing wherever the new mapping took effect. The `tlbi` was never at fault, and the
earlier "suspect CPUECTLR_EL1.SMPEN" note was wrong.

EL0 code and stack now live in a linker-placed, 2 MiB-aligned `.el0` region, and only that region is
granted EL0 access. The EL0 task consequently cannot call kernel print functions at all, so it reports
its verdict through a syscall argument - the first place the EL0/EL1 boundary is real rather than
notional.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | 2 -> 3 (+1) | `el0_region` reads the linker's `__el0_start`/`__el0_end`; taking a linker symbol's address does not dereference it. |
| `arch/aarch64/usermode.rs` | 9 -> 11 (+2) | The `svc #2` verdict call from EL0, and the EL0 stack now living in `.el0.data`. |

## 2026-08-03 - Pi 4 milestone 6: EL0 and the svc path (feat/pi4-aarch64)

Dropping to EL0, taking `svc` back to EL1, returning a value, and exiting cleanly.

The syscall number is read from `ESR_EL1`'s `imm16`, **not from a register**, so userspace cannot claim
a different call by clobbering a register on the way in. Arguments still arrive in registers and are
untrusted exactly as §18 requires. The demo checks the round trip in both directions rather than only
that it did not crash.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/usermode.rs` | new, 9 | The EL0 entry (`msr sp_el0` / `elr_el1` / `spsr_el1` then `eret` - all three must be set before the eret; an unset `SP_EL0` faults on the first push and points at the user code rather than the entry), the demo task's `svc` calls, the exit path's context switch back to the saved kernel context, module-owned static setup, and one unreachable `wfe` park. |
| `arch/aarch64/exceptions.rs` | 5 -> 7 (+2) | The synchronous-lower-EL dispatcher: `mrs esr_el1` to read the exception class and `imm16`, and dereferencing the trap frame the vector assembly built on the current stack. |
| `arch/aarch64/mmu.rs` | 2 -> 3 -> **2** (net 0) | `allow_el0` was added here and then REMOVED: it patched `AP` on the live map and invalidated the TLB, and on hardware the `tlbi` never returned. EL0 access is now decided when the tables are built, before translation is on, so there is no live mutation and no maintenance. Net effect on the count is nil. |

## 2026-08-03 - Pi 4 milestone 5: context switch (feat/pi4-aarch64)

Saving and restoring the AAPCS64 callee-saved set, proven by two kernel tasks ping-ponging.

The context switch is the first piece here that can corrupt state **silently**: a wrong byte offset or
a dropped register does not fault, it resumes a task holding someone else's value and the damage
surfaces later somewhere unrelated. So it is proven on its own, and the demo *checks* rather than
merely runs - each task holds witnesses in callee-saved integer and FP registers across the switch and
reports a mismatch. A test that cannot fail is worth nothing (A9-1).

`d8`-`d15` are saved even though skipping them would look harmless: the kernel is built for a target
with FP/SIMD, LLVM emits NEON for bulk copies without being asked (the Pi 2 hit this with `memcpy`),
and omitting them yields corruption only when a switch lands between a NEON spill and its reload.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/context.rs` | new, 2 | (1) `global_asm!` for the switch itself - a context switch cannot be expressed in Rust. (2) The `switch` wrapper is `unsafe fn` and calls the extern; its contract names what the caller owes (valid non-aliasing contexts, `next` either previously saved or prepared by `init`). |
| `arch/aarch64/ctxdemo.rs` | new, 7 | Demo-only. Six are `switch` calls plus context/stack setup on statics this module owns during single-threaded boot; one is the `wfe` park on an unreachable path. The statics are `TaskContext`s and two 16 KiB `.bss` stacks, sized once and visible (§26.6.1). |

## 2026-08-03 - Pi 4 milestone 4: GIC-400 + generic timer (feat/pi4-aarch64)

A periodic tick: the standard GICv2 the Pi 4 has (unlike the Pi 2's bespoke Broadcom controller, this
is spec-driven and transfers to any GICv2 board) plus the architectural generic timer. The IRQ vectors
gained a RETURN path - previously every vector reported and halted, which for a tick is not a tick.

Two disciplines carried over from the 32-bit port's scars: the counter frequency is **read from
`CNTFRQ_EL0`, never assumed** (a hardcoded guess made every sleep on that board wrong by orders of
magnitude and hid for months), and the wait for the first ticks is **bounded** so a timer that never
fires reports instead of hanging the boot looking busy.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/gic.rs` | new, 4 | Four Device-mapped MMIO accessors: `init` (quiesce, priority mask, enable distributor + CPU interface), `enable` (priority, target, set-enable bit), `acknowledge` (GICC_IAR - a read WITH the architectural side effect of acknowledging), `eoi` (GICC_EOIR). Each is a single volatile access to a register the GICv2 spec defines. |
| `arch/aarch64/timer.rs` | new, 5 | System-register access only: `CNTFRQ_EL0` and `CNTPCT_EL0` reads (side-effect-free), `CNTP_TVAL_EL0`/`CNTP_CTL_EL0` writes to arm and disable, and `msr daifclr, #2` to unmask IRQs at EL1. EL2 granted EL1 access to the physical timer during the boot drop (CNTHCTL_EL2), so none of these trap. |

## 2026-08-03 - Pi 4 milestone 3: exception vectors (feat/pi4-aarch64)

`VBAR_EL1` and a 16-entry vector table. This comes before the timer or the GIC because until it exists
a fault is a silent lockup: the core takes the exception and branches into whatever sits at the reset
vector. Every milestone after this adds code that can fault, so this is what makes the rest debuggable
(invariant 12 applied to the port itself).

The table is proven by **raising a real `brk #0`**, not by asserting the register took. A self-test that
cannot fail is worth nothing - the lesson from A9-1, where an odd sentinel let a test pass without the
fix it was meant to prove.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/exceptions.rs` | new, 5 | (1) `global_asm!` for the 2 KiB-aligned table and the shared save-and-report tail - assembly is the only way to express a vector table. (2) `msr vbar_el1` in `init`, which only changes where exceptions are taken to. (3) `mrs vbar_el1` in `installed`, side-effect-free. (4) `mrs esr_el1`/`far_el1` in the reporter, likewise. (5) dereferencing the trap frame the vector assembly just built on the current stack - valid for the call, unaliased. |
| `arch/aarch64/mod.rs` | 27 -> 28 (+1) | The deliberate `brk #0` that proves the vectors fire. It never returns: with the table installed it is taken to the synchronous handler, which reports and halts. |

## 2026-08-03 - Pi 4 milestone 2: the MMU (feat/pi4-aarch64)

Identity-mapping the low 4 GiB and turning translation on. 4 KiB granule, 39-bit VA (so the walk starts
at L1 and there is no L0 to carry), 2 MiB blocks at L2 - one L1 plus four L2 tables, exactly 20 KiB of
`.bss`, a fixed and visible footprint (§26.6.1).

The RAM/device split is the part that matters for correctness. Device memory is mapped
**Device-nGnRnE** and never-execute; RAM is Normal write-back, inner-shareable. Mapping MMIO as Normal
does not fail cleanly - the core may reorder, merge or speculatively repeat accesses to a peripheral
register, and the symptom is a device behaving erratically rather than a fault that names the mapping.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mmu.rs` | new, 2 | (1) One block builds the tables and installs them: plain stores to two `.bss` statics this function owns exclusively while the MMU is still off and no other core runs, then `msr` of MAIR/TCR/TTBR0, a `tlbi vmalle1` (the TLB is architecturally UNKNOWN out of reset, so entering with whatever it holds is a real hazard), and finally `SCTLR_EL1.M|C|I`. Because the map is identity, the instruction after the `isb` executes translated at the same address. (2) `is_enabled` reads `SCTLR_EL1` back - a side-effect-free system-register read - so the boot reports what actually happened rather than asserting the write took. |
| `arch/aarch64/mod.rs` | 25 -> 27 (+2) | `pl011_init` (UART disable/drain/line-control/enable) and `gpio_init_uart` (mux GPIO14/15 to ALT0, pull-up on RX). Both are identity-mapped MMIO writes during single-threaded boot; the BUSY drain is bounded so a wedged UART cannot hang the boot (invariant 12). |

## 2026-08-03 - Raspberry Pi 4 (aarch64) bring-up + the stale arch stubs

Milestone 1 of the Pi 4 port, plus the arch stubs it exposed as out of date.

**The stubs had drifted.** The neutral kernel gained `arch::imp` items over time (`note_user_task`,
`page_tables::finalize_service_address_space`, `page_tables::free_page_table_root`, and from the v0.9.0
console work `fb_commit` + `FB_READBACK_CHEAP`) without every arch stub being updated, so the
demarcation builds `docs/multi-arch.md` claims had quietly stopped compiling. The Pi 4 build surfaced
it: the compiler names the surface a port still owes, which is the workflow `arch/CLAUDE.md` describes.
All five arch stubs are current again and compile clean.

Each stub's `+2` is the two `unsafe fn` DECLARATIONS (the keyword is what the scanner counts) - both are
empty no-ops, since a stub has no address spaces to finalize or free. They carry `# Safety` contracts
matching the x86/ARM implementations so a real port inherits the obligation rather than discovering it.

| File | Change | Why |
|------|--------|-----|
| `arch/aarch64/mod.rs` | 23 -> 25 (+2) | The two `unsafe fn` page-table stubs. The boot path was also reworked for the Pi 4 (EL2 -> EL1 drop, BCM2711 PL011 at 0xFE201000, a bounded TXFF wait) but that is net-neutral on the count: `CurrentEL` read and the UART poll replace the old unguarded byte writes. |
| `arch/loongarch64/mod.rs` | 23 -> 25 (+2) | The two `unsafe fn` page-table stubs. |
| `arch/riscv64/mod.rs` | 23 -> 25 (+2) | The two `unsafe fn` page-table stubs. |
| `arch/riscv32/mod.rs` | 23 -> 25 (+2) | The two `unsafe fn` page-table stubs. |
| `arch/s390x/mod.rs` | 18 -> 20 (+2) | The two `unsafe fn` page-table stubs. |

## 2026-08-03 - the A9-1 large-page guard selftest (test/large-page-guard)

Kernel-audit A9-1 fixed a walk that could write into a mapped data frame, but the trigger (a large page
covering a `map_in_active_tables` target) does not occur on any machine available, so the guard was
unproven. `selftest_large_page_guard` manufactures the condition rather than mocking it: it installs a
real 2 MiB `PS=1` entry over an unused canonical VA, calls the real `map_in_active_tables` inside it,
and checks the mapped frame is byte-for-byte untouched. Verified in **both** directions - the test
reports `CORRUPTION at u64 index 1` with the guard removed, and PASS with it restored.

Feature-gated behind `mmio-map-fault-test`; a default build does not compile it (confirmed: the default
image never prints `pt-selftest`, and identity is 24/24).

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/page_tables.rs` | 48 -> 51 (+3) | (1) `asm!` reading CR3 to find the active PML4 - a register read with no memory effect. (2) One block covering the test body: filling the frame with a sentinel, building PML4/PDPT/PD, installing the large-page entry, running the walk, checking the sentinel, tearing the entry down and `invlpg`-ing it. The VA is confirmed unmapped by `entry_for_va` before anything is written, so no live mapping is touched, and the frame comes exclusively from the allocator. (3) `free_phys_frame` after the entry is cleared and flushed, so no page-walker can still reach it. Permitted layer (§18.1), each block SAFETY-commented. |

## 2026-08-01 - one framebuffer console for every arch: unsafe 11 -> 3 (refactor/fbcon-neutral)

`arch/x86_64/fb.rs` (790 lines) and `arch/arm/fbcon.rs` (512 lines) were two independent
implementations of the same terminal. They are now one neutral module, `kernel/src/fbcon/`, with a thin
per-arch backend. **`fbcon/` is NOT a permitted layer (§18.1), and no amendment was sought to make it
one** - instead the design was changed so the neutral module needs no `unsafe` at all.

The mechanism: the arch hands the console the framebuffer as a **`&'static mut [u8]` slice**
(`FbParams::mem`) rather than a base address. Every pixel write in `fbcon/` is then a bounds-checked
slice write; the scroll memmove is `slice::copy_within`; the clear is `slice::fill`. The single
`unsafe` per arch is `core::slice::from_raw_parts_mut` at init, in the layer that actually knows the
mapping is valid and permanent.

**This supersedes a claim in the 2026-07-10 entry below**, which said of the fast blit path: *"There is
no safe route ... a bounds-checked `&mut [u32]` would defeat the purpose (a compare per pixel is the
very overhead removed)."* That is false, and the assumption behind it was that the check would be
per-pixel. It is not: slicing the row once (`mem.get_mut(off..end)`) bounds-checks the whole run, and
`chunks_exact_mut` then iterates it without further checks - the same contiguous store pattern, with
one compare per row instead of per pixel. Verified behaviour-identical: the rendered framebuffer is
**byte-for-byte the same PNG** (md5 `2670c31a...`) before and after the change, on the `edit`
full-screen path that exercises glyphs, reverse video, absolute positioning and erase.

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 5 -> 2 (-3) | Reduced to the Limine binding + the WC store fence. Remaining: the `sfence` in `fb_commit`, and `from_raw_parts_mut` over Limine's reported `[address, address + pitch*height)` - higher-half, copied into every address space by `PageTable::new`, so 'static; taken once on the BSP before any AP starts, and no other kernel reference to the framebuffer exists, so `&mut` exclusivity holds. |
| `arch/arm/fbcon.rs` | 6 -> 1 (-5) | Reduced to the pixel format, the D-cache clean in `fb_commit`, and the single-writer mirror gate. Remaining: `from_raw_parts_mut` over the mailbox-reported base, mapped by `video::map` before this runs, taken once on the boot core before the tick or any AP starts. |
| `fbcon/mod.rs`, `fbcon/render.rs` | 0 | Neutral layer - unsafe-free by construction, which is why no §18.1 amendment is needed. |

Net: 11 lines -> 3, and roughly 300 lines of duplicated logic collapsed to one copy.

---

## 2026-07-29 - LAN9514 networking: interrupt-driven RX, PHY link state, SNTP (feat/arm-usb-interrupt)

The Pi 2's onboard LAN9514 brought up on real hardware, then reworked from a polled RX to an
**interrupt-driven** one (ping loss 85% -> 4%), plus a real PHY link read and the SNTP wall clock. All in
the permitted `arch/` layer; every block SAFETY-commented, all of it core-0 + IRQ-masked driver state
(the RX ring, the reassembly buffer, the DMA burst buffer) or MMIO through the existing `rd`/`wr` helpers.

The RX path's blocks are the bulk of it: a single-producer/single-consumer ring (`net_rx_ring_push`,
`net_rx_ring_pop`), the cross-burst reassembly buffer (`net_rx_parse`, `net_rx_partial_reset`), arming and
re-arming the background bulk-IN (`net_rx_async_arm`, `net_rx_async_start`, `net_rx_isr`), and the
consumer syscall (`net_frame_rx`), plus `net_rx_ring_full` - the backpressure test that stops the ISR
re-arming into a full ring (receiving frames only to drop them is work done for nothing), and
`hotplug_poll` - the hub-port watcher (present but deliberately NOT wired to a caller; see the note at the
idle hook in `arch/arm/mod.rs` for why enumeration cannot run there yet).
Producer and consumer are mutually exclusive without a lock because
both run on core 0 with IRQs masked - the syscall path cannot park, and ARM IRQ entry masks IRQs - which
is the argument each SAFETY comment carries.

This entry also **re-baselines two files whose drift predates this work**: `irq.rs` and `mod.rs` are
untouched by these commits (verified: identical counts at the branch point) and had accumulated
undocumented blocks from the earlier USB/SD bring-up on this branch. They are recorded here so the
inventory matches source again rather than carrying a known-stale baseline forward.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | 14 -> 34 (+20) | Interrupt-driven net RX (ring push/pop, burst parse + reassembly, partial reset, background arm/re-arm, the halt ISR, the consumer syscall), the net-up arm sites, and the PHY link poll's read of `ASYNC_BULK.active` (the guard that keeps a link poll from destroying a parked storage transfer). The +1 over the earlier 33 is the SAME guard read in `link_poll`, the idle-context twin of that poll (a cable change now reports itself without being asked). |
| `arch/arm/irq.rs` | 11 -> 13 (+2) | **Pre-existing drift, re-baselined:** blocks added by earlier branch work (USB IRQ routing/ack), not by the networking commits. |
| `arch/arm/mod.rs` | 43 -> 44 (+1) | **Pre-existing drift, re-baselined:** one block from earlier branch work. |

## 2026-07-24 - SD card bring-up on real hardware: mailbox power + base clock + pin routing (feat/pi2-arm32)

The Pi 2's SD card init failed on hardware while passing under emulation. Three board-level steps the
Arasan EMMC needs, none of which the driver can do itself (it is granted only its controller's 4 KiB
register window - §12.3), so they live in the kernel's boot path like the USB power-on before them:

- **`video::set_sd_power_on`** - mailbox `SET_POWER_STATE` device 0 (SD card). The EMMC's registers
  answer even when the card's power domain is off, so skipping it gives exactly the observed symptom:
  registers read fine, no command completes. QEMU stubs the tag, so it is invisible there.
- **`video::read_emmc_clock` / `emmc_clock_hz`** - mailbox `GET_CLOCK_RATE` clock id 1. The base clock
  MUST come from the platform: the Arasan's capability register reports it wrongly on this SoC (Linux
  marks it `missing_caps`), and a guessed divider runs the card's identification clock at the wrong
  speed, which no card answers.
- **`sd_route_to_emmc`** (mod.rs) - route GPIO 48-53 to ALT3 (the Arasan) with the BCM2835 pull-up
  sequence, and log what the firmware had them set to first (ALT0 = the other `sdhost` controller,
  which would leave the Arasan electrically disconnected from the card).

All in the permitted `arch/` layer, each block SAFETY-commented, all single-threaded boot-path MMIO or
the caches-off mailbox buffer.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/video.rs` | 11 -> 17 (+6) | `set_sd_power_on` (MBOX fill + response read, +2), `read_emmc_clock` (MBOX fill + response read + `EMMC_CLOCK_HZ` store, +3), `emmc_clock_hz` (static read, +1). |
| `arch/arm/mod.rs` | 42 -> 43 (+1) | `sd_route_to_emmc` - GPFSEL4/5 read-back + ALT3 write and the GPPUD/GPPUDCLK1 pull sequence for GPIO 48-53. |

## 2026-07-24 - underline cursor on the ARM framebuffer console (feat/pi2-arm32)

The TV console had no visible cursor (x86 draws an underline; ARM did not). `fbcon` now paints a 2 px
underline at the write position, lifts it before the position moves, and honours `ESC[?25l`/`ESC[?25h`
so full-screen apps can hide it. The cursor is moved ONCE per write run rather than once per byte, so
bulk output (a `help` listing, a scroll) does not pay for it - which is why `put_bytes` gained its own
`unsafe` block instead of looping over the public `put_byte`: one borrow of the `FBCON` static for the
whole run. `arch/arm/fbcon.rs` unsafe 4 -> 5 (+1), permitted arch layer, SAFETY-commented, same
single-writer justification as the existing blocks (serialized by `pl011_write`'s SERIAL_BUSY guard).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/fbcon.rs` | 4 -> 5 (+1) | `put_bytes` borrows the `FBCON` static once for a whole run (lift cursor, render every byte, repaint cursor) instead of re-borrowing per byte via `put_byte`. |

### `console_boot_complete` on ARM (2026-07-26)

The shell calls `console_boot_complete()` to dismiss the boot screen and stop mirroring kernel log
output to the TV, so the prompt is not overwritten by a late service log (`fs: journal recovered ...`
landing on the `gsh> ` line - the reason the prompt appeared only after pressing Enter). x86 has always
implemented it; ARM's was an empty stub. Implementing it needs one borrow of the `FBCON` static to
clear the screen and home the cursor.

| File | unsafe | Why |
|------|--------|-----|
| `arch/arm/fbcon.rs` | 5 -> 6 (+1) | `clear_and_home` borrows the `FBCON` static once to clear the framebuffer and reset cursor/write position. Same single-owner discipline as `put_bytes`: core 0 holds `SERIAL_BUSY` when the console writes, and the function returns immediately when no framebuffer was mapped (`base == 0`). Permitted arch layer (§18.1), SAFETY-commented. |

## 2026-07-24 - full ARM keyboard decode: new `arch/arm/hid.rs`, and dwc2 unsafe SHRINKS (feat/pi2-arm32)

Full HID boot-keyboard support for the ARM in-kernel USB driver (numeric keypad, arrows/navigation,
F-keys, Caps Lock latch, Ctrl+letter, typematic auto-repeat) moved into a new **`arch/arm/hid.rs`
containing ZERO `unsafe`** - it is pure decode logic (usage code -> bytes), no MMIO and no statics.
The driver's old `decode_report` + its `PREV_KEYS` `unsafe` block are gone, replaced by a single
`KBD_STATE` struct accessed inside `poll`'s ALREADY-EXISTING `unsafe` block, so `dwc2.rs`
**shrinks 15 -> 14**. A new file with no unsafe and a net reduction in the driver.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/hid.rs` | new, 0 | Pure HID decode logic (keymap, CSI sequences, auto-repeat timing). No `unsafe`: no MMIO, no statics - the caller owns the state and passes it in. |
| `arch/arm/dwc2.rs` | 15 -> 14 (-1) | `decode_report`'s `PREV_KEYS` block removed; the keyboard state (`KBD_STATE`) is now read inside `poll`'s existing `unsafe` block instead of a second one. |

## 2026-07-24 - DWC2 split debug + VideoCore USB power-on + board MAC (feat/pi2-arm32)

Two `arch/arm/` files gained `unsafe` this session; all blocks are in the permitted arch layer (§18.1),
each carries a `// SAFETY:` comment, and each is single-threaded (core-0-only) hardware access.

- **`dwc2.rs` 13 -> 15 (+2):** the split-transaction microframe **trace** added while diagnosing the
  low-speed-keyboard split XactErr. `trace_split` captures `(phase, HFNUM, HCINT, GNPTXSTS, GINTSTS)` into
  a fixed `SPLIT_TRACE` array (write), and the one-shot split-fail dump reads it back (read). Both are a
  bounded fixed array indexed `< SPLIT_TRACE_MAX`, single-writer on core 0. (The HubAddr/PrtAddr fix, the
  multi-packet split loop, the toggle-parity fix, and the driver-review fixes are all **safe** code -
  `rd`/`wr`/atomics - and added no `unsafe`.)
- **`video.rs` 6 -> 11 (+5):** `set_usb_power_on` (the VideoCore `SET_POWER_STATE` mailbox call that
  powers the DWC2 AXI master - the breakthrough that made USB work on real silicon) is one MBOX-fill
  block; `read_board_mac` is three (MBOX fill, MBOX response read, and the `BOARD_MAC` static **write** -
  a plain pre-MMU store, NOT an atomic, because ARMv7 `STREXD` is UNPREDICTABLE before the MMU is on);
  `board_mac` is one (the `BOARD_MAC` static read). These read the board Ethernet MAC (`GET_BOARD_MAC`,
  tag 0x00010003) since the Pi 2 has no EEPROM.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | 13 -> 15 (+2) | `SPLIT_TRACE` fixed-array capture (`trace_split` write + split-fail dump read) for the low-speed-keyboard split diagnosis. |
| `arch/arm/video.rs` | 6 -> 11 (+5) | `set_usb_power_on` (USB-HCD power mailbox, +1); `read_board_mac` (MBOX fill + read + `BOARD_MAC` pre-MMU store, +3); `board_mac` (`BOARD_MAC` read, +1). |

## 2026-07-23 - BCM2835 GPIO (feat/pi2-arm32)

Added `gpio_op(op, pin)` - drive a SoC GPIO pin's direction/level/read (BCM2835 GPFSEL/GPSET/GPCLR/GPLEV in
the already-Device-mapped peripheral window). Exposed via a new **capability-gated** `Gpio` syscall (45),
GPIO carries the UART/SD lines so it is granted only to the `shell` (GPIO_DEVICE cap, id 11), with a `gpio`
shell command (`gpio <input|output|high|low|read> <pin>`). QEMU-verified: drive pin 21 high -> read 1, low
-> read 0. `arch/arm/mod.rs` unsafe 41 -> 42 (+1, one SAFETY-commented block of GPIO MMIO). Other arches
stub `gpio_op` to `-1` (no unsafe).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 41 -> 42 (+1) | `gpio_op` drives the BCM2835 GPIO (GPFSEL/GPSET/GPCLR/GPLEV MMIO). |

## 2026-07-23 - BCM2835 hardware RNG (feat/pi2-arm32)

Added a hardware entropy source: `hw_random()` reads the BCM2835 SoC RNG (one-time enable + bounded wait
for a word, then read `RNG_DATA`) in the already-Device-mapped peripheral window. Exposed ungated as
InspectKernel query 19 (entropy confers no authority, like the raw TSC) with a `random` shell utility as
its consumer. QEMU-verified (`random 3` returns three distinct u32s). `arch/arm/mod.rs` unsafe 40 -> 41
(+1, one SAFETY-commented block of RNG MMIO). The other arches stub `hw_random` to `None` (no unsafe; x86
RDRAND is a trivial follow-up).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 40 -> 41 (+1) | `hw_random` reads the BCM2835 RNG (RNG_CTRL/STATUS/DATA MMIO). |

## 2026-07-23 - BCM2835 watchdog reset (feat/pi2-arm32)

`hardware_reset()` on ARM was a stub that spun, so the shell `reboot` (and Ctrl+Alt+Del) hung the Pi 2
instead of resetting it. Implemented the BCM2835 power-management watchdog reset: one `unsafe` block of
volatile 32-bit writes to `PM_WDOG`/`PM_RSTC` in the already-Device-mapped peripheral window, gated by the
`0x5A` password (the documented reset poke). QEMU-verified (a `reboot` re-runs the kernel from its boot
banner). `arch/arm/mod.rs` unsafe 39 -> 40 (+1), permitted arch layer, SAFETY-commented.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 39 -> 40 (+1) | `hardware_reset` does the BCM2835 PM watchdog reset (PM_WDOG/PM_RSTC MMIO writes). |

## 2026-07-23 - Soundness audit of the DWC2 USB unsafe (feat/pi2-arm32)

The session took `arch/arm/dwc2.rs` from 3 to 13 `unsafe` blocks (the whole USB stack: DMA control/bulk
transfers, cache maintenance, the keyboard/net/storage device paths) plus 3 SDK ABI wrappers
(`net_frame_tx`/`rx`/`info`). Audited all of them - a self-review plus an **independent adversarial pass**
(a second reviewer briefed to find UB, checked against the actual concurrency machinery: `arm_irq_dispatch`,
`stub_svc`, `NEUTRAL_SCHED` boot ordering, `uart_rx_poll`). **Verdict: no memory-safety bug (UB / OOB /
data race).** The load-bearing facts, each verified against code not comments:

- **`&mut *addr_of_mut!(DMA)` never overlaps.** The three accessors (`ctrl_xfer`/`bulk_xfer`/`poll`) are all
  **core-0 only** (MPIDR gate on `poll`'s call site + `on_core0()` on the net syscalls) and **mutually
  exclusive in time**: `poll` runs from the timer IRQ; the net syscalls run IRQs-masked (SVC `cpsid i`) and
  **never block**, so the timer cannot fire mid-transfer. During *boot enumeration* `poll` is unreachable -
  `arm_irq_dispatch` routes the tick to the demo scheduler until `NEUTRAL_SCHED` flips *after* `dwc2::init`
  (this closes the one real-looking race: the hub walk keeps issuing `ctrl_xfer`s after `KBD_READY`/
  `NET_READY` are set mid-walk). This invariant is now documented at the `DMA` static.
- **Device-controlled lengths are all bounded before a copy** (`bulk_xfer` HCTSIZ residual only *shrinks*
  `recv`; `net_frame_rx` smsc `flen` hard-checked `4 + flen > got`; every descriptor-parse loop guards
  `i + k <= total`). No transfer exceeds the 2048-byte scratch buffer. No `MPS0 == 0` div-by-zero.
- **`NET_MAC` / `PREV_KEYS` statics** are boot-single-writer (Release) / core-0-single-accessor; `net_info`
  reads `NET_MAC` after an Acquire on `NET_READY`. **SDK wrappers** pass a 32-bit pointer + length <= 1600
  (no ABI truncation) and the kernel range-checks every user pointer.

**Two latent robustness gaps hardened (P1/P2, neither reachable by any current caller or device input):**
`ctrl_xfer`'s OUT path could panic (`&data[..n]` with `n = dlen.min(2048)`, not `.min(data.len())`) and
programmed the DMA length unclamped to the scratch buffer. Both now clamp to `min(d.data.len(), data.len())`
- symmetric with `bulk_xfer` - so a future caller with `dlen > data.len()` can neither panic nor DMA past
the buffer. No new `unsafe` (edits to an existing block + comments); `dwc2.rs` stays at **13**. One liveness
(not safety) note recorded: a wedged controller spins the core IRQs-off for the bounded `wait_halt` timeout.

## 2026-07-23 - DWC2 USB keyboard: DMA mode + hub enumeration + HID poll (feat/pi2-arm32)

The slave/PIO experiment (entry below) got control transfers working on QEMU's DWC2 model *only for the
SETUP stage*: QEMU emulates the DWC2 **DMA** engine, not slave/PIO, so DATA-IN never delivered bytes.
Reverted enumeration to **internal DMA mode** - which is also how u-boot/Linux drive the Pi 2 core - and
completed the keyboard: point `HCDMA` at the `DMA` scratch static, bracket every transfer with cache
maintenance (`flush_dcache`, DCCIMVAC + `dsb`, the A7's DMA is not coherent), enumerate the hub the
keyboard sits behind (the Pi 2's LAN9514 topology, and QEMU's NEC-hub model), select boot protocol, and
poll the interrupt IN endpoint from the timer tick (`poll` -> `decode_report` -> `console_push_byte`).
QEMU-verified end to end: keys typed on the emulated `usb-kbd` reach the shell. The DMA buffer address is
the VideoCore bus alias `0xC000_0000 | phys` on real hardware and identity (`0`) under QEMU, selected by
the new `qemu` cargo feature (`scripts/arm_build.py --qemu`); the shipped image stays hardware-correct.

So `dwc2.rs` unsafe returns to **3 -> 8**: the DMA path is back (`flush_dcache`'s 2 asm blocks + the
`DMA`-static access in `ctrl_xfer` and `poll`), plus one `PREV_KEYS`-static access in `decode_report` (the
edge-trigger previous-report buffer, reached via `addr_of_mut` to avoid a mutable-static reference). All
in permitted `arch/`; every block carries a SAFETY comment.

**Bulk transfers (+1 -> 9):** the same session added `bulk_xfer` (the USB bulk-transfer primitive, the
shared foundation for USB mass storage and later USB-Ethernet), which accesses the `DMA` scratch static
the same way `ctrl_xfer`/`poll` do (`addr_of_mut`, core-0 only, cache-bracketed). Verified in QEMU end to
end against `usb-storage` (READ CAPACITY + READ(10) of a planted block-0 signature). The BOT/SCSI layer on
top of it (`bot_command`, `probe_mass_storage`) is all safe code. So `dwc2.rs` is **9**.

**CDC-ECM USB-Ethernet (+1 -> 10):** the same session added a CDC-ECM driver (`configure_cdc_ecm` +
`net_verify_arp`) - raw ethernet frames over the bulk endpoints, verified in QEMU by an ARP round-trip
through `usb-net`. The one added `unsafe` is a single write of the station MAC into the `NET_MAC` static
(`addr_of_mut`, core-0 enumeration only); the MAC is otherwise passed as a local, and the frame build +
BOT/SCSI code is all safe. So `dwc2.rs` is **10**.

**USB-net bridge to userspace (+2 -> 12):** the same session added the mechanism the userspace ARM
`nic-driver` calls (`net_frame_tx`/`net_frame_rx`/`net_info`, syscalls 42-44) to move ethernet frames to
the in-kernel CDC-ECM device. Two added `unsafe`: `on_core0` reads MPIDR (`mrc`, side-effect-free) to
guard the single-channel DWC2 against off-core access; `net_info` reads the `NET_MAC` static
(`addr_of`, read-only). Both in permitted `arch/`. So `dwc2.rs` is **12**. (`net_verify_arp` was
removed - net-stack now drives networking end to end.)

**Multi-device USB + smsc95xx (+1 -> 13):** the same session made keyboard/ethernet/storage coexist (per-
device channel selection, all safe) and added the `smsc95xx` (Pi 2 LAN9514) driver. The one added `unsafe`
is a second `NET_MAC`-static write (in `configure_smsc95xx`, core-0 enumeration only) - the register R/W,
PHY/MDIO, and TX/RX-framing code is all safe. So `dwc2.rs` is **13**.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | 3 -> 13 (+10) | DMA reinstated (`flush_dcache` +2, `DMA`-static in `ctrl_xfer`/`poll`/`bulk_xfer` +3, `PREV_KEYS`-static +1, `NET_MAC`-static write +1), USB-net bridge (`on_core0` MPIDR read +1, `NET_MAC`-static read in `net_info` +1), smsc95xx (`NET_MAC`-static write in `configure_smsc95xx` +1). Slave-mode FIFO code (all safe `rd`/`wr`) removed. |
| `arch/arm/dwc2.rs` | 13 -> 15 (+2) | Slave/PIO pivot for the real Pi 2 v2.80a core (the internal DMA master never dispatches; `qemu`-gated, DMA kept for QEMU). `pio_out` reads the DMA scratch via a raw ptr to push into the TX FIFO (+1); `pio_in` writes it via a raw ptr while draining the RX FIFO (+1). Both are the identity-mapped scratch, bounded by `len` (SAFETY-commented). Higher layers (`ctrl_xfer`/`bulk_xfer`) unchanged. |
| `arch/arm/dwc2.rs` | 15 -> 13 (-2) | PIO pivot reverted for the full u-boot DMA transcription (back to DMA on both platforms, u-boot's exact config): `pio_out`/`pio_in`/`slave_wait_halt` removed, so the two raw-ptr FIFO blocks are gone. Back to the DMA-scratch access already counted. |

---

## 2026-07-23 - DWC2 control transfers via slave/PIO mode (feat/pi2-arm32)

The DWC2's internal DMA master never initiated a transfer on the Pi 2: across a dozen HW tests the channel
armed (ChEna set), the host framed (HFNUM advanced) and every config register read correct, yet
`GRSTCTL.AHBIdle` stayed 1 and `HCDMA` never advanced. Switched enumeration to **slave / PIO mode** - the
mode every working bare-metal Pi USB driver uses: DMA disabled (`GAHBCFG.DmaEn=0`), OUT data pushed
word-by-word into the NP TX FIFO and IN data popped from the RX FIFO after `GRXSTSP`, no bus-mastering.
This **removed** the DMA scratch static, the `flush_dcache` cache-coherency bracket, and the tick-driven
state machine, so `dwc2.rs` unsafe **shrank 8 -> 3** (only the `rd`/`wr` Device-MMIO accessors + the `nop`
`spin` remain; the slave-mode transfer code is all safe `rd`/`wr`). Enumeration is now synchronous (a
one-time bounded boot cost).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | 8 -> 3 (-5) | Slave/PIO rewrite dropped the DMA path: removed `flush_dcache` (DCCIMVAC + `dsb`, -2) and `poll_inner` + the two step handlers' `DMA`-static access (-3). Remaining: `rd`/`wr`/`spin`. |

---

## 2026-07-22 - DWC2 USB host bring-up, increment 1 (feat/pi2-arm32)

The Pi 2's USB is a Synopsys DesignWare USB 2.0 OTG (DWC2) core. `dwc2.rs` brings it up in host mode and
detects the attached device (the first step toward a USB keyboard): read the Synopsys core ID, soft-reset
the core, force host mode, power + reset the root port, report the connected device's speed. All new
unsafe is Device-mapped MMIO (the DWC2 register block is inside the already-Device-mapped peripheral
window) plus a `nop`-spin, both permitted `arch/`. QEMU (`-M raspi2b,usb=on -device usb-kbd`): core
GSNPSID=0x4f54294a, device detected + port enabled at full-speed.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | new, 3 -> 8 | `rd`/`wr` (DWC2 Device MMIO 32-bit accessors) + `spin` (`nop` delay) for bring-up; increment 2 adds `flush_dcache` (DCCIMVAC + `dsb` - the DMA cache-coherency bracket, 2) and, in the tick-driven state machine, `poll_inner` + the two step-completion handlers' access to the `DMA` scratch static (identity-mapped physical buffer, 3). |
| `arch/arm/mod.rs` | 38 -> 39 (+1) | `uart_rx_poll` reads MPIDR to gate the USB `dwc2::poll()` to core 0 (it is the single writer of the DWC2 channel + DMA). |

## 2026-07-22 - ARM serial input works: idle + scheduler-context fixes (feat/pi2-arm32)

The core-0 block-path idle bug (typing did nothing) is fixed. `wait_for_interrupt` was a bare `wfi` that
never re-enabled IRQs, and the scheduler context was seeded with cr3=0 because the timer preempted the
bootstrap before `run(0)` seeded it; masking IRQs before arming the neutral scheduler closes that race.
New unsafe is the `clrex` before the serial-lock acquire (exclusive-monitor hygiene) and the `cpsie i`
added to the idle `wfi`, both permitted `arch/`.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 36 -> 38 (+2) | `clrex` before the `SERIAL_BUSY` compare-exchange (clear a stale ARMv7 exclusive-monitor that wedged the shell's 2nd console echo); GPIO14/15 -> ALT0 mux in `gpio_init_uart` so serial RECEIVE works, not just transmit. |

## 2026-07-16 - SEC-21 security fix (feat/hardening)

| File | Change | Why |
|------|--------|-----|
| `memory/allocator.rs` | 43 → 44 (+1) | **SEC-21:** new safe `zero_frame(phys)` helper (one `unsafe` `write_bytes` block via the HHDM alias) so the AllocMem syscall can zero a frame before it becomes user-readable, closing a cross-task info leak (`alloc_frame` returns un-zeroed frames). Permitted `memory/` layer with a SAFETY comment; keeping the `unsafe` here lets the caller (`syscall/dispatch.rs`, a grandfathered file) stay `unsafe`-free per §18.5. |

SEC-4 (bounds-checking the SDK `Dma`/`Mmio` wrappers) adds **0** to this inventory: the SDK's
permitted-layer `unsafe` is not tracked here (see the intro), and the change adds only safe `assert!`
bounds checks, not new `unsafe`. SEC-5 (fs subtree revoke) is `unsafe`-free service code.

## 2026-07-22 - HDMI framebuffer on the Pi 2, Phase 2: text console (feat/pi2-arm32)

`fbcon.rs` renders the serial stream onto the framebuffer (glyphs via the shared `noto-sans-mono-bitmap`
font), so the boot log + `gsh>` prompt appear on the TV; `pl011_write` mirrors to it under the same
SERIAL_BUSY guard. New unsafe is the framebuffer pixel writes + the single FBCON static, permitted
`arch/`. QEMU screendump: text renders (231 distinct colours in the top-left region = antialiased glyphs).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/fbcon.rs` | new, 3 | `put_pixel` (device-mapped framebuffer store), the FBCON static in `init` + `put_byte` - the glyph renderer + cursor. |

## 2026-07-22 - HDMI framebuffer on the Pi 2, Phase 1 (feat/pi2-arm32)

Toward x86-parity local console. The ARM has no Limine to hand it a framebuffer, so `video.rs` asks the
VideoCore GPU for one via the mailbox property interface, `mmu::map_framebuffer` maps it Device, and a
solid fill proves the pipeline (QEMU screendump: a clean 1024x768 blue). New unsafe is the MMIO/mailbox
and framebuffer writes (video.rs) and the live-L1 mapping + TLB flush (mmu.rs), both permitted `arch/`.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/video.rs` | new, 4 | VideoCore mailbox (Device MMIO), framebuffer writes, MBOX static access - the framebuffer acquisition + fill. |
| `arch/arm/mmu.rs` | 6 -> 8 (+2) | `map_framebuffer`: write the framebuffer's Device sections into the live L1, then clean the D-cache + TLBIALL so the walker sees them. |

## 2026-07-22 - AP bring-up: park a mis-identified core (feat/pi2-arm32)

Real Pi 2: releasing core 3 brought up a core whose MPIDR read back as 0 - it registered as a SECOND
core 0, two cores ran scheduler::run(0), raced, and one crashed the boot (UNDEF halt). `ap_boot_main`
now parks any core that finds its own id ALREADY ready (a confused/duplicate release), so the system
boots reliably on the good cores. +1 unsafe: the `wfi` park loop.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 35 -> 36 (+1) | Park (`wfi`) a released AP whose id is already ready - the mis-identified-core guard. |

## 2026-07-22 - AP bring-up: vectors-first + barrier (feat/pi2-arm32)

On real HW core 3's bring-up intermittently faulted BEFORE it installed its vectors, so with VBAR still 0
it branched into low memory (an UNDEF at 0x618) and halted the boot. `ap_boot_main` now installs the
per-core vectors FIRST (before ACTLR.SMP/MMU) so any bring-up fault is REPORTED through the vectors
instead of wandering, plus a `dsb sy`/`isb` to synchronize with core 0's published boot state (SEC-25/28
weak-ordering hygiene). +1 unsafe: the barrier block.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 34 -> 35 (+1) | `dsb sy`/`isb` barrier at the top of `ap_boot_main` (weak-ordering sync before an AP relies on core 0's tables/arenas). `install_for_core` moved ahead of the MMU enable so bring-up faults are loud, not wild. |

## 2026-07-22 - ARM frame reclaim on task death (feat/pi2-arm32)

The ARM kill path reclaimed nothing (`reclaim_user_frames` was a `{ 0 }` stub) and the neutral kill path
`free_frame`d the page-table root - fine on x86 (root = a general frame) but on ARM the root is an ARENA
L1 slot, so it corrupted the frame bitmap (the `alloc_frame returned kernel-range frame` panic on the
first respawn). Real reclaim: `reclaim_user_frames` walks the dying task's L1/L2, `free_frame`s its USER
pages (AP[1:0] >= 0b10; distinguished from shared kernel hole-fill pages) and returns its own L2s to the
arena; the arenas gained per-slot `used` flags so freed L1/L2 slots are reused (a `free_frame`-of-root
would still corrupt, so the root goes back to the arena via `free_page_table_root`). QEMU-proven: 15
logger kill/restart cycles, 0 panic, 0 leak/exhaustion (`freed 76 frames` each, was `freed 0`).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 25 -> 27 (+2) | `reclaim_user_frames` (walk L1/L2, free USER pages + L2 slots) and `free_page_table_root` (return the L1 root to the arena) - the two `unsafe fn` bodies that free a dead task's memory. `alloc_l1/l2` gained CAS-on-`used` + `free_l1/l2` (safe: atomic store + `addr_of`). |
| `arch/x86_64/page_tables.rs` | 47 -> 48 (+1) | `free_page_table_root` = `free_frame` (behaviour-identical to the old inline neutral root free), so the neutral kill path can be arch-neutral for both ISAs. |

## 2026-07-22 - Fault-survival on ARM: kill the faulting task, keep the kernel alive (feat/pi2-arm32)

The data/prefetch abort handlers went from report-and-halt to the x86 C2/A14/A15 property: a USER-mode
(PL0) fault kills just that task and reschedules; a kernel fault still reports and halts. `stub_dabt` /
`stub_pabt` now branch on the faulting mode (SPSR & 0x1f == 0x10) - no new `unsafe` (asm inside the
existing naked blocks). The +1 is the `wfi` guard loop in the new `arm_user_fault_kill`, which calls the
neutral `kill_current()` (sets the task Dead, `yield_current` switches to the next task). HW-verifiable;
QEMU-proven: `spawn greet` (rigged to read address 0) -> "user task faulted ... killing it; kernel
continues", and the shell + ping/pong keep running, no panic.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | 23 -> 24 (+1) | `arm_user_fault_kill` (reached from the abort stubs in SVC mode on the faulting task's kernel stack) logs the kill loudly and calls `kill_current()`; the +1 is its `wfi` guard loop (kill_current does not return for a Dead task). |

## 2026-07-22 - SMP: cores 1-3 online on the Pi 2 (feat/pi2-arm32)

Bring the other three Cortex-A7s online. All new `unsafe` is in the permitted `arch/arm/` layer (§18.1),
each block SAFETY-commented. QEMU-verified: `smp: 4 cores ready`, services placed on cores 0+1, cross-core
IPC flowing, 0 faults. (Weak-memory-ordering hardening SEC-25..28 for real HW is a documented follow-up.)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 33 -> 34 (+1) | Core-3 lost-wakeup fix: a periodic re-`sev` in `smp_bringup`'s AP-ready wait re-arms the event line for a core that entered WFE just as the first SEV fired (its mailbox is still set, so it proceeds on the nudge). HW-proven: cores 1-2 came up but core 3 hung on release until this landed; now all 4 A7s come up on the Pi 2. The +1 is the `dsb`/`sev` block. (Diagnostic breadcrumbs used to find this were removed once understood.) |
| `arch/arm/mod.rs` | 26 -> 33 (+7) | `get_lapic_id` reads MPIDR (the core id - the linchpin for `current_core_id`); `ap_entry` (naked AP entry: HYP-drop, VFP, per-core stack); `ap_boot_main` (ACTLR.SMP + `mmu::enable_on_this_core` + vectors + timer, one asm block); `smp_bringup` (D-cache clean before release + per-core mailbox-3 SET write + `dsb`/`sev`). The `arm_ap_park` release loop is `global_asm!` (not counted as a Rust `unsafe` block). |
| `arch/arm/mmu.rs` | 4 -> 6 (+2) | Split `enable` into `build_tables` + `enable_on_this_core` (a `pub unsafe fn`, +1) so each AP loads the SAME L1 into its TTBR0; core 0 calls it too. The register-write blocks are unchanged; the +2 is the new unsafe fn wrapper and core 0's call site. |
| `arch/arm/exceptions.rs` | 21 -> 23 (+2) | `install_for_core(core)` gives each AP its OWN banked ABT/UND/IRQ/FIQ stacks (BSS `AP_MODE_STACKS`) instead of the shared linker-symbol stacks - two cores taking a timer IRQ at once would otherwise corrupt the one IRQ stack. The +2 are the raw-pointer stack-top computation and the VBAR/banked-SP asm block. |
| `arch/arm/irq.rs` | 10 -> 11 (+1) | `this_core()` reads MPIDR so the dispatch reads THIS core's `CORE_IRQ_SOURCE`/`CORE_TIMER_IRQCNTL` (`+4*core`), and `start_tick_ap` routes each AP's own timer. The +1 is the MPIDR read. |

## 2026-07-22 - The interactive shell on ARM (feat/pi2-arm32, increment 5)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 23 -> 26 (+3) | Real console I/O: `console_write_bytes_gated` -> `pl011_write` (output); a PL011-RX -> input-ring path (`pl011_rx_drain`, `uart_rx_pop`, `uart_rx_poll`, `uart_rx_drain_now`, `console_push_byte`, `set_input_ready`/`input_ready`) so the shell reads serial input via ConsoleRead. The +3 unsafe are the three MMIO/ring blocks (`pl011_rx_drain`, `uart_rx_pop`, `console_push_byte`). |
| `arch/arm/exceptions.rs` | unchanged count | `stub_svc` now saves/restores the caller's USER-banked `SP_usr`/`LR_usr` around the syscall (asm inside the existing naked block, no new `unsafe`). A syscall that blocks (recv/console_read) switches to another USER task, which clobbers the shared USER bank; the shell, woken from `console_read`, resumed on the logger's shallow SP and faulted just above the stack top. Saving on the task's own kernel stack (like `stub_irq`'s trap frame) fixes it. |

**The interactive shell runs on ARM.** `gsh> ` prompt, reads serial input, echoes, and executes
commands: `help` prints the command list, `version` prints `GodspeedOS 0.7.0`. 0 faults. The
committed increments are unregressed (IPC 6600+ messages, supervisor bootstrap - both 0 faults - with
the `stub_svc` USER-bank change). New ARM boot: `arm-shell` (`sched_shell.rs`); the shell is built for
ARM (`arm_built += shell`). x86 unchanged (all changes in `arch/arm/`; the shell-spawn helper is
`#[cfg(target_arch = "arm")]`).

## 2026-07-21 - The NEUTRAL spawn works on ARM (feat/pi2-arm32, increment 4a)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 23 -> 25 (+2) | `finalize_service_address_space(cr3)` - the arch hook the neutral spawn calls after building a service page table: clones the kernel identity into it + cleans the D-cache (ARM has no shared higher-half kernel). The `unsafe fn` + its block. |
| `arch/x86_64/page_tables.rs` | 46 -> 47 (+1) | The x86 `finalize_service_address_space` is a `pub unsafe fn` no-op (kernel is shared higher-half); the empty `unsafe fn` is the +1. |
| `arch/arm/mod.rs` | 22 -> 23 (+1) | `syscall_slot` now returns a real per-core `PerCoreSyscallData` (an `addr_of_mut` unsafe) instead of null: the neutral spawn commits `is_user=true`, and `prepare_ring3_switch` writes through this pointer for every user task. Also added the safe `note_user_task` hook (`irq::mark_task_user`; no unsafe). |
| `task/mod.rs` | unchanged (7, at the grandfathered floor) | The neutral `spawn_service_with_config` gained ONE line calling `finalize_service_address_space`, and `arm_spawn_logger_neutral` (an ARM-only pub probe). The finalize call was folded into the existing `unsafe { TaskContext::new_user }` block so `task/`'s floor holds (§18.5) - no amendment. `scheduler::commit_task` gained a safe `note_user_task` hook call (no unsafe). |

**The neutral spawn machinery runs unchanged on ARM.** `task::spawn_service_with_config` - the exact path
the supervisor's spawn syscall uses (ELF load, user-stack + ctx-page map, kstack-pool alloc, cap
minting, ServiceContext write) - spawns the `logger` on ARM: `task: 'logger' spawned OK on core 0
(slot 0)` -> `logger: ready`. The two ARM-specific steps are now arch-seam hooks the neutral code calls
itself (both no-ops on x86, so x86 is byte-for-byte unchanged - verified it still compiles). This is the
foundation the supervisor stands on. Gated behind `arm-sched-spawn`.

## 2026-07-21 - Atomic syscalls + CLREX on ARM (feat/pi2-arm32, increment 3b hunt cont'd)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 9 -> 10 (+1) | `arm_irq_dispatch` reads the interrupted CPSR from the trap frame (`frame_sp + 68`) to implement **atomic syscalls**: skip timer preemption when a USER task is in SVC (a syscall), since preempting ARM kernel code mid-syscall corrupts (SPSR_svc + SVC-banked sp are shared). Gated on `ARM_TASK_IS_USER[slot]` (set by `mark_task_user`) so a *kernel* task running in SVC stays preemptible. The +1 unsafe is the frame read. |
| `arch/arm/context_switch.rs` | unchanged count | Added `clrex` at the top of `switch_context`: a voluntary switch does not implicitly CLREX like an exception entry, so a task switched out mid-`ldrex`/`strex` could leak the exclusive monitor and wedge a SpinLock. Inside the existing naked block, no new `unsafe`. |
| `arch/arm/mod.rs` | unchanged count | Doc-only: `syscall_slot` stays null (ARM tracks user tasks arch-locally, so the neutral `prepare_ring3_switch` never runs and never derefs it). |

**Status: the mid-syscall preemption FAULT is fixed (verified: no EXCEPTION over 30 s, `sched_demo`/
`sched_user` still rotate/preempt); the IPC still hangs on a residual corruption across the voluntary
syscall-context `switch_context`** (`block_and_reschedule`'s `slot` local, asserted `< MAX_TASKS` at
entry, reads back garbage at the tail). Full diagnosis in `sched_ipc.rs`. The SPSR-window fix (`stub_svc`
`cpsid i`) from the prior commit stays.

## 2026-07-21 - Cross-service IPC wiring (feat/pi2-arm32, increment 3b - WIP, blocked on a diagnosed bug)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/spawn.rs` | unchanged count | Refactored to expose `load_service_raw(elf, extra_caps)` + `map_stack_and_ctx` (shared with `sched_ipc`): load any service ELF, map its stack/ctx, reserve a slot, install `LOG_WRITE` + caller-supplied endpoint caps, leaving the ctx write + `fill_kernel_identity` to the caller. `load_logger_into_slot` now rides on it. No net `unsafe` change; `load_service_raw`'s block is the cap inserts. |
| `arch/arm/sched_ipc.rs` | 6 -> 9 (+3) | Rewritten from the 2-logger frame proof to a real `ping`->`pong` IPC attempt: create pong's endpoint (the `spawn_service_with_config` sequence - `alloc_endpoint_id` + register resource/routing/name), mint a RECV cap for pong and a SEND cap for ping, hand-build both `ServiceContext`s (`write_ipc_ctx`), and commit both as scheduled USER tasks. The +3 `unsafe` is `write_ipc_ctx` (raw ctx writes), `commit_user`, and the `halt` WFI. `build.rs` now builds `ping`/`pong` for `armv7a-none-eabi`. Gated behind `arm-sched-ipc`. |

**Status: the wiring is correct, the runtime is blocked on a diagnosed kernel bug.** Verified: the ctx
is wired correctly (a dump confirmed `send_peer_count=1, peer0.slot=1, name="pong"`), both services reach
PL0 and log (`ping: starting`, `pong: ready on core 0`). But once `ping` loops issuing syscalls
alongside a second running user task, a task's registers are corrupted and it jumps to a wild PC (garbage
syscall numbers, then a DATA ABORT to `0xfffffeae` from a PC in a data page). Bisected: `pong`'s blocking
`recv` alone is fine; `ping` ALONE (self-scheduling) loops forever clean; `ping`+`pong` together corrupts.
So the fault is in a **real cross-task `switch_context` reached from a *syscall* context** (yield/block) -
a path #1/#2/#3a never exercised (they switch only via the timer IRQ; #3a's two user tasks busy-loop on a
non-blocking `recv`, never yielding). The USER-banked `SP_usr`/`LR_usr` were ruled out (saving them in both
`stub_svc` and `switch_context` left the corruption unchanged, so those attempts were reverted). Next
leads: the AAPCS callee-saved contract across a syscall-context switch between two different user address
spaces (TTBR0 change), or SVC-stack nesting when the timer preempts a task mid-syscall. The 2-logger
banked-frame proof it replaced is preserved in commit `3e6cb3f`; the banked frame (`stub_irq`) stays, and
ping+pong both reaching PL0 still exercises it. The committed increments (#1/#2/#3a) are unregressed
(default `preempt selftest PASS 9/9/9`, sched-user `logger: ready`).

## 2026-07-21 - Two USER services at once: the banked-register trap frame (feat/pi2-arm32, increment 3a)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | unchanged count | `stub_irq` now stacks the interrupted task's USER-banked `SP_usr`/`LR_usr` (`stmdb r0, {sp, lr}^` on save, `ldmia r0, {sp, lr}^` on restore) - the prerequisite for **more than one** user task. With one, nothing else touched the USER bank across a round trip; with two, task B's ring-3 execution would clobber task A's user stack unless it is saved per task. The extra instructions live in the existing naked block, so no new `unsafe`. Frame grew 16 -> 18 words. |
| `arch/arm/context.rs` | unchanged count | `TrapFrame` gains `usr_sp`/`usr_lr` (matching the 18-word layout) and `prepare_task` zeroes them for kernel tasks (their USER bank is unused). Struct/field change only, no new `unsafe`. |
| `arch/arm/page_tables.rs` | unchanged count | L1 arena 2 -> 8, L2 arena 16 -> 64: the boot loader selftest takes one L1 and each live service takes one, so two only left room for a single service. Sized (bounded static, §26.6.1) for the running service set - IPC pair, supervisor, shell. Constants only, no `unsafe`. |
| `arch/arm/sched_ipc.rs` | 0 -> 6 (new file) | Loads **two** logger instances as scheduled USER tasks (each its own address space) plus two kernel spinners, and runs them under `scheduler::run`. The isolation test for the banked frame; grows into real send/recv (increment 3b). The `unsafe` is the static-stack setup and the `new_user`/`new_kernel`/`commit_task` calls. Gated behind `arm-sched-ipc`. |

**What this proves.** Two independent USER services run concurrently in ring 3 under the scheduler and
both reach PL0 and issue their cap-validated syscall (`logger: ready` twice) with no corruption and no
fault. Were the banked frame wrong, the second user task's ring-3 execution would clobber the first's
`SP_usr` and one would fault - so "both ready, no fault, system live" is the proof the per-task
`SP_usr`/`LR_usr` save/restore is correct. This is the trap-frame foundation IPC stands on. Verified in
QEMU (`raspi2b`); the default image is unregressed (`preempt selftest PASS 9/9/9`).

## 2026-07-21 - A USER service runs through the scheduler, preemptively (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context_switch.rs` | 14 -> 13 (-1) | `new_user` is now **real**: it builds a context whose first `switch_context` drops to PL0 via a new `user_entry_trampoline` (installs the user stack, fabricates a USR-mode SPSR with IRQs on, `movs pc`). The loud `user_mode_unimplemented` stub it replaced is deleted - net one fewer `unsafe`. `switch_context` also gained a `TLBIALL` on the TTBR0-change branch (SEC-26/27: an ARM address-space switch does not implicitly flush), inside the existing naked block (no new `unsafe`). |
| `arch/arm/page_tables.rs` | 21 -> 23 (+2) | `clean_invalidate_dcache_all` (set/way `DCCISW`) moved here from `spawn.rs` as the shared home for cache maintenance: it makes a service's page-table descriptors visible to the non-cacheable walker once at spawn, so `switch_context` needs no per-switch cache work. The `unsafe fn` + its asm block are the +2. |
| `arch/arm/spawn.rs` | unchanged count | Refactored to expose `neutral_bootstrap` + `load_logger_into_slot` (shared with `sched_user`) and to call `page_tables::clean_invalidate_dcache_all` rather than a private copy. No net `unsafe` change. |

**What this proves.** A real GodspeedOS service (`logger`) runs *through* the neutral scheduler on ARM,
not entered directly: loaded into its own address space, committed to a task slot, and preempted in
ring 3 by the timer (its trap frame lands on its own kernel stack), while spinning kernel tasks are
round-robined around it. `logger: ready` is its cap-validated syscall, issued from PL0 under its own
TTBR0 - so the per-task page table, the `switch_context` TTBR0-swap + `TLBIALL`, and the one-shot
descriptor D-cache clean all hold end to end. Verified in QEMU (`raspi2b`): `logger: ready` once,
kernel tasks advancing past tick 3, no fault. **Single** user task by design: a second one needs
`stub_irq` to also stack the banked `SP_usr`/`LR_usr` (the next increment); with one, nothing else
touches the USER bank across the round trip. The default image is unregressed (`preempt selftest PASS
9/9/9`, `neutral surface PASS`).

## 2026-07-21 - Timer preemption via the neutral scheduler (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 8 -> 9 (+1) | `arm_irq_dispatch` now routes the timer tick to the neutral `timer_tick_from_irq` (the preemptive `switch_context` path) once `NEUTRAL_SCHED` is set, instead of the early `context.rs` demo scheduler. The +1 unsafe is the `timer_tick_from_irq` call. |
| `arch/arm/sched_demo.rs` | unchanged count | The demo tasks now **spin** (no yield) and arm `NEUTRAL_SCHED`, proving the timer preempts a non-cooperating task. |

**The mechanism, and why the same IRQ stub serves both paths.** The stub saves the full interrupted
frame on the task's kernel (SVC) stack and calls `arm_irq_dispatch`, which returns an `sp` the stub
adopts (`mov sp, r0`). The early demo scheduler returns a *different* task's frame to adopt. The
neutral `timer_tick_from_irq` instead does the `switch_context` INTERNALLY - it swaps `sp` to the next
task's kernel stack itself - so `arm_irq_dispatch` returns `frame_sp` unchanged, and after this task is
later resumed (`switch_context` unwinding back into the call), `frame_sp` again names THIS task's
frame, making the `mov sp` a no-op. One stub, two mechanisms.

**Non-yielding tasks are genuinely preempted**, proven by output interleaved mid-print (a tick caught a
task between arbitrary instructions). The boot `preempt_selftest` still uses the demo path
(`NEUTRAL_SCHED` defaults false) and still passes 9/9/9, so the default image is unregressed. This is
the preemption real services need (they block on `recv`, they do not yield).

## 2026-07-21 - Neutral scheduler runs tasks on ARM (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/sched_demo.rs` | 0 -> 6 (new file) | Commits three kernel tasks and enters the neutral `scheduler::run(0)`; it round-robins them (A->B->C->A...) via `pick_next` + `switch_context` + `yield_current`. Proves the neutral scheduler - the foundation the supervisor and every service stand on - runs on ARM. The `unsafe` is the static-stack setup, the `new_kernel`/`commit_task` calls, and the BootInfo construction for the neutral bootstrap. Gated behind `arm-sched-demo`. |

**Cooperative first, deliberately.** The tasks `yield` (a scheduling point), so this exercises the
scheduler's task table + `switch_context` without the timer-preemption rework - running the timer IRQ
on per-task kernel stacks so it can `switch_context` a *non-yielding* task - that real services need.
That is the next increment; this proves the layer beneath it. No per-task page tables (all tasks share
the kernel identity map), so a switch never changes TTBR0 and the D-cache dance from the service spawn
does not arise.

## 2026-07-21 - Minimal service spawn (feat/pi2-arm32)

Increment 6 groundwork: enough to load a real service, set up its task + capability, and run it at
PL0 issuing syscalls. All in the permitted `arch/` layer with SAFETY comments; no grandfathered floor
moves (the one neutral helper, `set_current_task`, is a **safe** fn - the atomic store is not UB - so
`scheduler.rs` stays at its floor).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/spawn.rs` | 0 -> 7 (new file) | The minimal spawn: build a BootInfo, run the neutral `memory`/`percpu`/`capability` init, load the logger ELF, map its user stack + service-context page, reserve a task slot with a `LOG_WRITE` cap, clone the kernel into the service address space, switch TTBR0, and drop to PL0. |
| `arch/arm/page_tables.rs` | 19 -> 21 | `fill_kernel_identity`: clone the live kernel identity map into a service page table (whole-section where the service slot is empty; page-fill the L2 where the service tabled-over a kernel section, so kernel data sharing the ctx/code 1 MiB stays reachable). |
| `arch/arm/mod.rs` | 21 -> 22 | The `interrupts` module's `disable`/`enable`/`local_irq_save`/`restore`/`wfi` are now REAL (`cpsid`/`cpsie`/`wfi`), the ARM `read`/`write_user_bytes`/`validate_user_ptr` are real, `switch_to_boot_stack` sets SP, and ACTLR.SMP is set at boot. |

**MILESTONE REACHED: `logger: ready`.** A real GodspeedOS service, loaded from an ELF, runs
unprivileged (PL0) on 32-bit ARM under its own address space, and logs through a capability-checked
`svc` into the neutral dispatcher. `clean_invalidate_dcache_all` (a set/way `DCCISW` sweep, +1
unsafe) before the TTBR0 switch was the final fix: the kernel maps its memory as 1 MiB **sections**
but the service maps the shared 1 MiB as 4 KiB **pages**, and stale D-cache lines from the section
view made the cap-table spinlock's `LDREX`/`STREX` fail under the page view. Cleaning the D-cache
makes every line coherent before the walker and exclusive monitor see the new mappings, and the lock
acquires. Gated behind `arm-spawn-logger` so the default image still boots to the selftest halt; the
feature build runs the service to `ready`.

## 2026-07-21 - ARMv7 user mode / PL0 (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/usermode.rs` | 0 -> 15 (new file) | **A task runs UNPRIVILEGED for the first time.** Enters USR mode (PL0), runs a stub that cannot touch kernel memory, and has it `svc` back. The `unsafe` is: `enter_user` (the fabricated exception return that drops to PL0), `resume_boot` (restores the kernel context on the magic svc), the user stub, I-cache sync for the copied code, the ATS1CPUR/W unprivileged translation probes, and the frame copy/map in the selftest. |
| `arch/arm/page_tables.rs` | 19 (unchanged) | `l2_small_page` now encodes PL0 access from the `USER` flag: AP=0b11 (PL0 RW), 0b10 (PL0 RO), 0b01 (PL0 none) - the page's whole two-level security model. No new `unsafe`. |
| `arch/arm/exceptions.rs` / `syscall.rs` | unchanged counts | The SVC entry publishes the caller's SPSR (so a syscall can see its privilege), and the magic test syscall routes to `on_magic_svc`. |

**Entering USR mode is a fabricated exception return.** No `iret`: set SPSR to USR (IRQs enabled), set
LR to the entry PC, arrange the USR banked SP (via a brief system-mode switch), and `movs pc, lr` -
which copies SPSR->CPSR and LR->PC atomically, dropping privilege. The ARM analogue of x86's IRETQ.

**The proof of PL0 is the SPSR at the svc, not that the code ran.** The CPU records the caller's mode
in SPSR_svc; `SPSR.mode == 0x10 (USR)` is unforgeable evidence the stub executed unprivileged. The
selftest checks exactly that (observed 0x10), and separately probes the permission model with the
*unprivileged*-access translation ops (ATS1CPUR/W): user code is user-readable, user stack
user-writable, and a KERNEL page is NOT user-accessible - isolation, proven non-faulting. Getting back
out with no scheduler: `enter_user` saves the kernel context first; the magic svc restores it.

## 2026-07-21 - ARMv7 SVC syscall entry (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/syscall.rs` | 0 -> 5 (new file) | **The SVC syscall entry** - `svc #0` traps into the neutral `syscall_handler`. The `unsafe` is `arm_svc_dispatch` (forwards to the `unsafe` neutral handler) and the `issue_svc`/selftest asm. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | The SVC vector went from report-and-halt to a real entry: save `LR_svc`/`SPSR_svc`, call the dispatcher, `movs pc, lr` to return restoring CPSR. No new `unsafe` block - the naked stub was already one. |

**Two ARM-specific things had to be right, and one was a bug.** (1) SVC targets SVC mode and the
kernel already runs in SVC, so `LR_svc`/`SPSR_svc` are saved *first thing* like a nested exception, or
the next `bl` clobbers the return address; done that way the entry works from a USR caller (real
tasks) and an SVC caller (the selftest) alike. (2) **The 32-bit ABI bug:** `syscall_handler` takes
`u64` parameters, and on 32-bit ARM each `u64` is a *register pair* (number in r0:r1, arg0 in r2:r3,
rest on the stack). Passing the four `r0-r3` values to a `u64`-parameter function read the arguments
shifted - it showed up as a wrong echo (7400 vs 7345). `arm_svc_dispatch` takes `u32`s (one register
each, matching r0-r3) and widens to `u64` for the neutral call; **most** syscall arguments on this arch
(pointer, handle, cap slot, length) fit in 32 bits, so the widening is loss-free for them. The **one
exception** is a value that can exceed 32 bits - a `recv_timeout` in generic-timer ticks - which the
single-register ABI would truncate; its SDK wrapper pre-clamps it on ARM (userspace-audit A-U1). The ABI
convention + this constraint are now documented for SDK/service authors in `arch/arm/CLAUDE.md`. That
widening is the seam the SDK port uses.

**No user tasks yet (increment 3)**, and the real handlers touch per-task state, so the selftest
proves the *entry mechanism* through a test dispatch (a mix of all four args, so a correct result
proves each survived the mode switch) and leaves `syscall_handler` wired for when tasks arrive. A
second trap confirms the path is re-entrant.

## 2026-07-21 - Neutral frame allocator live on ARM (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/meminit.rs` | 0 -> 4 (new file) | **Wires the neutral `memory::init` on ARM** - the first shared (non-arch-layer) subsystem running on 32-bit ARM, and the prerequisite for per-task page tables and service spawn. Builds a `BootInfo` from the DTB memory map + linker kernel bounds, reserves the kernel image as a low region, and runs the neutral bitmap allocator. The `unsafe` is the `static mut MEM_REGIONS`/`BootInfo` construction, the `__fiq_stack_top` linker-symbol read, and reconstructing a `Frame` to free in the selftest. |
| `memory/allocator.rs` + 7 arch `page_tables` | guard relaxed + `PHYS_IS_IDENTITY` const | The allocator panicked on `hhdm == 0` ("HHDM offset not set"). That is true on x86/Limine but WRONG on ARM: the kernel runs identity-mapped, so hhdm=0 is the correct value (`hhdm + phys == phys` already addresses the frame). Fixed the boundary-correct way (`arch/CLAUDE.md`): each arch declares `page_tables::PHYS_IS_IDENTITY` (true on ARM, false elsewhere), and the guard only fires where a zero offset genuinely means "unset". x86 identity 24/24 confirms the shipping arch is unaffected. |

**`memory::init` fit the neutral allocator unchanged** because two ARM facts line up with what it wants:
`hhdm=0` works (identity map), and `protect_kernel_page_table_frames` - the one Limine-table-specific
step - already returns early when `hhdm == 0`, a clean no-op rather than a special case. Result on
hardware-shaped input: `frame allocator ready (946 MiB free)`, and the selftest allocates 8 distinct
page-aligned frames, checks the free count drops by 8, frees them, and checks it returns to baseline.

## 2026-07-21 - ARMv7 two-level page tables (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 0 -> 17 (new file) | **Real two-level 4 KiB page tables**, replacing the compile-only stub inline in `mod.rs`. `mmu.rs` gave 1 MiB sections; this gives an L2 table under an L1 entry, so individual pages carry their own permissions. The `unsafe` is: the TTBR0/TLB primitives (`invalidate_tlb_page` = TLBIMVA, `read`/`write_page_table_base`), the descriptor writes into the live and fresh L1/L2 tables, the static-arena table allocators, and the `ATS1CPR`/`ATS1CPW` translation probes the selftest uses. |

**The read-only proof needs no fault.** `ATS1CPW` runs a privileged-*write* address translation and
reports the result in `PAR.F` - so a read-only page returns "denied" for a write while `ATS1CPR`
(read) still returns its address. The selftest maps one page RW and one RO into the live tables and
checks: both translate for read, RW is writable, **RO is not**. The negative is the load-bearing one
(same discipline as the MMU and IOMMU selftests): "RW translates" only shows the L2 was built; "RO
refuses a write" shows the AP/APX permission bits are actually enforced. That is real per-page
protection, the thing 1 MiB sections could not give.

**The frame source is a bounded static arena, deliberately.** x86's `PageTable::new` pulls table
frames from the neutral `alloc_frame`, which needs `memory::init` + a real memory map - and that pulls
in Limine-shaped assumptions (`protect_kernel_page_table_frames`) that are a separate integration
step. So table memory is a fixed static arena here (§26.6.1), with the `alloc_frame` swap called out as
the one remaining seam. The *algorithm* - build an L2, point an L1 entry at it, encode the page with
its permissions - is the real one the neutral path will drive unchanged. `map_in_active_tables` fills a
currently-unmapped L1 slot (a VA in the gap between RAM end and the peripherals) rather than converting
a live section, so running code is never momentarily unmapped.

## 2026-07-21 - Neutral context-switch surface, real (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context_switch.rs` | 0 -> 14 (new file) | **The real `arch::imp::context_switch` surface** the neutral scheduler imports: `TaskContext`, `new_kernel`/`new_user`, and the naked `switch_context`, plus a selftest driving a kernel task through that exact neutral API. The `unsafe` is the two naked asm fns (switch + first-entry trampoline), the constructors, `user_mode_unimplemented`, and the selftest's static/ptr manipulation. Replaces the compile-only stub that was inline in `mod.rs`. |
| `arch/x86_64/context_switch.rs` + 5 arch stubs | +1 line each (`TaskContext::ZERO`) | A neutral leak fixed by an arch primitive, not a special case (`arch/CLAUDE.md`). The scheduler built a zero context with a literal `TaskContext { rbx: 0, ... }`, naming x86 registers in neutral code - which does not compile once ARM's `TaskContext` is ARM-shaped. Each arch now exposes `const ZERO: Self`; the scheduler uses `TaskContext::ZERO`, naming no register. |

**Kernel-only integration, honestly scoped.** The neutral scheduler spawns **only** ring-3 tasks
(`new_user`, from ELF binaries) - it has no path that calls `new_kernel` - so `scheduler::run()`
end-to-end genuinely needs userspace, which ARM does not have (0-byte service placeholder). What *is*
provable kernel-only is the scheduler's core primitive: `TaskContext::new_kernel` + `switch_context`
driving an ARM kernel task, which the selftest exercises through the neutral types.

**The switch mirrors x86 semantics exactly, and a bug proved it.** Like x86, the ARM switch saves
callee-saved + `sp` + `lr` but **not** `cr3` (it only *loads* TTBR0). The first selftest faulted in a
loop with TTBR0=0 - because `SCHED_CTX.cr3` stayed zero from its `ZERO` init, and switching back
loaded it. That is the *exact* gotcha x86 documents at `scheduler.rs` ("seed the scheduler context's
CR3 ... switch_context never saves CR3, only loads it"); reproducing it confirms the semantics match.
The fix seeds `SCHED_CTX.cr3` with the live TTBR0, as the neutral `run()` does. `new_user` builds a
context that halts loudly if entered - ring-3 needs an SPSR return, per-task page tables, and SVC
syscalls, none of which exist yet, so a premature user spawn fails visibly rather than running undefined.

## 2026-07-20 - Device tree parsing: learn the memory map (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dtb.rs` | 0 -> 6 (new file) | **Flattened Device Tree parsing.** Six SAFETY-commented blocks, all bounds-checked reads of the firmware-supplied blob: big-endian u32 reads, node-name comparison, and reading `DTB_PTR` itself. Every offset is checked against the blob's own declared `totalsize` before being walked - a corrupt header pointing outside the blob is exactly how a parser wanders into unmapped memory. |
| `arch/arm/mod.rs` | +1 asm site (no new unsafe block) | `_start` now stashes `r2` (the DTB pointer) into `r10` before the mode check clobbers `r0-r2`, and publishes it into `DTB_PTR` **after** the BSS zero - which would otherwise wipe it. |

**Why this stops being optional here.** Every layer so far tolerated a hardcoded `RAM_END`, copied
from what the firmware told Linux, with a comment admitting that was not how a real port should learn
it. That is fine while nothing depends on it, and stops being fine the moment the neutral kernel's
frame allocator does: a wrong constant hands out frames backed by memory that does not exist. The
firmware already knows the answer and passes it in `r2`.

**FDT is big-endian on a little-endian CPU**, so every u32 needs swapping - the most common way to get
nonsense from this format, hence a single `be32` rather than byte-swapping at each site. The parser is
deliberately minimal: find `/memory`, read `reg`, stop. It does not pretend to general
`#address-cells` handling it has not implemented.

A missing or unparsable blob falls back to the old constant but **announces it** (invariant 12),
because a silently wrong memory size becomes allocator corruption much later, far from its cause.
Note QEMU cannot exercise the real path here: `-device loader` sets the PC without emulating the
firmware's r0/r1/r2 handoff, so `DTB_PTR` is 0 there and only hardware tests the parse.

## 2026-07-20 - ARMv7 PREEMPTIVE switch (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context.rs` | 4 -> 6 (+2) | **Preemptive switching.** Two further SAFETY-commented blocks: fabricating a trap frame on a fresh task's stack, and the WFI halt for a task that returns from its entry function. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | `stub_irq` grew from a five-register handler into a full trap-frame entry: `srsdb` + `cps` + `push {r0-r12, lr}`, dispatch, `mov sp, r0`, `pop`, `rfeia sp!`. No new `unsafe` - the naked stub was already one block. |

**Cooperative and preemptive switching are genuinely different problems**, and the difference is
AAPCS. A cooperative switch happens inside a function call, so the compiler has already spilled the
caller-saved half and ten registers suffice. A preemptive switch is *forced* between two arbitrary
instructions with anything live, so the **entire** register file plus the resume PC and `SPSR` must be
captured.

**The ARMv7 obstacle is register banking**: on IRQ entry the CPU is in IRQ mode, where the interrupted
mode's `sp` and `lr` are banked away and unreachable. `srsdb sp!, #0x13` reaches across that by
pushing `LR_irq`/`SPSR_irq` onto the *SVC* stack; `cps #0x13` then stands on the interrupted task's
own stack to save the rest. The frame therefore lives on **the task's own stack**, which is what makes
a switch cheap - the state is already parked where it belongs, so switching tasks is switching `sp`
and nothing else. The dispatcher returns the frame to resume; returning a *different* pointer is the
entire mechanism of preemption. `rfeia sp!` restores PC and CPSR atomically.

`TrapFrame`'s field order mirrors the push order and is as load-bearing as `Context`'s; a mismatch
would resume tasks with scrambled registers, looking like random corruption far from the cause. A
fresh task is started by fabricating its frame rather than special-casing "never run" in the switch -
the same trick as `Context::prepare`, one layer down. `SPSR` is set with **IRQs enabled**: a task
started with them masked would run to completion and never yield, silently killing preemption with no
error anywhere.

The selftest runs three tasks that never cooperate and checks **all three** were scheduled - a switch
that always picked one task, or worked once then wedged, would still show *a* task running.

## 2026-07-20 - ARMv7 kernel context switch (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context.rs` | 0 -> 4 (new file) | **Cooperative kernel-mode context switch.** The switch itself is a three-instruction naked fn (`stmia`/`ldmia`/`bx lr`); the remaining blocks are the ping-pong selftest driving it. Only ten registers are saved and that is not a shortcut: AAPCS makes `r0-r3`/`r12` caller-saved, so the compiler has already spilled anything live at the call site, leaving the switch responsible for `r4-r11`, `sp`, `lr` - the same division as the x86 side. |

`Context`'s field order is **load-bearing**: `stmia`/`ldmia` transfer in increasing register number
regardless of how the list is written, so the struct must read `r4..r11`, `sp`, `lr` under `repr(C)`.
Reordering the fields would silently restore registers into the wrong slots.

A fresh context is started by *fabricating* its `lr` as the entry point, so the ordinary restore path
starts it - no special case in the switch. The selftest checks the round trip rather than mere
arrival: the counter is incremented by the *other* context and read back, which only works if state
survives in both directions (a half-working switch that transfers control but corrupts registers is
the dangerous case).

**This is cooperative - called, not forced.** A preemptive switch from the timer IRQ must save the
*full* register file, because an interrupt can land between any two instructions with anything live.
That is the next increment. No address-space switch either: all contexts share the identity mapping,
and per-task `TTBR0` writes bring the SEC-26/27 TLB obligations with them.

## 2026-07-20 - BCM2836 interrupt controller / timer tick (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 0 -> 8 (new file) | **Routing the timer IRQ so the counter becomes a tick** - the prerequisite for preemption. Eight SAFETY-commented blocks: volatile read/write of the Device-mapped BCM2836 core-local block (routing + pending), `CNTP_TVAL` and `CNTP_CTL` writes to arm the timer, `CNTP_CTL` and `CPSR` reads for diagnostics, and `cpsie i` / `cpsid i` to unmask and mask IRQs. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | The IRQ stub changed shape without changing its count: it now saves `r0-r3, r12, lr`, calls the dispatcher and **returns** via `ldm sp!, {r0-r3, r12, pc}^` (the trailing `^` restores CPSR from SPSR atomically). It is the only exception in the port that returns rather than halting. |

**The tick selftest caught a real routing bug, and the diagnostics located it precisely.** The first
version counted **zero** interrupts. The follow-up print made the cause unambiguous: `CNTP_CTL` read
`0x5` (ENABLE set, IMASK clear, **ISTATUS set** - the timer was firing), `CPSR` showed SVC mode with
IRQs unmasked, yet the core-local pending register read `0x0`. Timer firing + interrupts enabled +
nothing pending means the timer was raising a source nobody was listening to.

**Cause: `CNTP_*` addresses the secure OR the non-secure physical timer depending on the CPU's
security state, and those are two different interrupt sources** - `CNTPSIRQ` (bit 0) and `CNTPNSIRQ`
(bit 1). The Pi firmware enters an ARMv7 kernel in HYP (non-secure), so hardware raises bit 1; QEMU's
`raspi2b` stub passes through the secure monitor into *secure* SVC and raises bit 0. Routing only the
non-secure bit therefore worked on neither in the same image. The fix routes and accepts **both**,
exactly as `_start` accepts either HYP or SVC entry: one image, either security state, no assumption
left to be wrong about.

## 2026-07-20 - ARMv7 generic timer (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/timer.rs` | 0 -> 4 (new file) | **ARM generic timer + BCM2835 System Timer.** Four SAFETY-commented blocks, all side-effect-free reads: `CNTFRQ` (the firmware-programmed frequency), `CNTPCT` via `mrrc` into a register pair (the 64-bit counter, with an ISB so the read is not reordered), a volatile read of the System Timer's counter-low register, and `local_reg` reading the BCM2836 core-local block (timer control + prescaler). The last two are in ranges `mmu.rs` maps as Device memory. |

**ARM needs no timer calibration** - `CNTFRQ` reports the frequency architecturally, so the whole
x86 PIT-calibration apparatus (and the ~1 second-quantum bug it existed to fix on the T630) has no
counterpart here.

**But `CNTFRQ` is still cross-checked, because it is firmware-programmed rather than
hardware-discovered.** It is an ordinary read/write register that firmware is *supposed* to set;
firmware that forgets leaves it 0 or wrong, and every duration derived from it is then silently
wrong - surfacing much later as mysterious timing bugs. The Pi carries a second, independent clock
(the BCM2835 System Timer, fixed at 1 MHz by hardware), so the selftest measures one against the
other over 100 ms and compares the result with what `CNTFRQ` claims. That turns "the register says
19.2 MHz" into "two independent clocks agree on how long a second is". A zero `CNTFRQ` is reported
loudly and degrades to the System Timer rather than computing nonsense (invariant 12).

**The cross-check immediately paid for itself: on the Raspberry Pi 2, `CNTFRQ` is wrong by 19.2x.**
Hardware reports `CNTFRQ = 19200000` while the counter measurably advances at 1 MHz. The BCM2836
feeds the generic timer through a core timer prescaler (`0x4000_0008`) at `source * prescaler / 2^31`;
firmware programs `0x06AAAAAB`, which divides the 19.2 MHz crystal to **exactly** 1 MHz, and then
never updates `CNTFRQ` - so the register still advertises the undivided crystal. Trusting it would
have made every delay and every scheduler quantum wrong by 19.2x, with the symptom appearing far from
the cause. **QEMU cannot reproduce this**: it does not model the prescaler (both registers read 0) and
its `CNTFRQ` is truthful, so only hardware could have caught it. `timer_hz()` therefore returns the
**measured** rate, never `CNTFRQ`, and the selftest distinguishes a deviation *explained* by the
prescaler (a known board quirk, reported and continued) from an unexplained one (a real failure).

## 2026-07-20 - ARMv7 MMU (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mmu.rs` | 0 -> 4 (new file) | **ARMv7 short-descriptor translation, 1 MiB sections.** Four SAFETY-commented blocks: filling the L1 table (`static mut`, boot-only, secondaries parked and MMU off so nothing is walking it); the enable sequence (TLB/BP/I-cache invalidate, DACR=client, TTBCR=0, TTBR0, then SCTLR.M, with the DSB/ISB pairs the ARM ARM requires); enabling caches afterwards; and `translate()`, which runs the CPU's own table walker via ATS1CPR and reads PAR. `translate` is deliberately safe to call on an address expected to be UNMAPPED - a failed walk sets PAR.F rather than raising an exception, which is what lets the selftest prove the table bounds anything. |

The MMU is the gate on task isolation, and it comes *after* the vectors on purpose: a bad mapping is a
translation fault, and without a vector table that fault is a silent hang rather than a printed
`translation fault (section) - NOT MAPPED`.

**The selftest checks a negative, not just a positive** (same reasoning as the x86 IOMMU selftest,
§22 Test 12): confirming that mapped addresses translate only shows the table is non-empty, so it also
confirms that an address outside every mapped range does **not** translate. The three checks are
mutually validating - a broken `translate()` that always failed would break checks 1-2, and a blanket
identity map would break check 3.

## 2026-07-20 - ARMv7 exception vectors (feat/pi2-arm32)

The 32-bit ARM port gains its vector table. All additions are in the permitted `arch/` layer with
`// SAFETY:` comments, so no §18.5 amendment is needed and no grandfathered floor moves.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | 0 -> 21 (new file) | **ARMv7 exception vectors.** Until this existed, ANY fault on ARMv7 was a silent lockup - no vector table means the CPU jumps to whatever sits at address 0 and wanders off, which is exactly the silent failure invariant 12 forbids. The count is dominated by the eight one-instruction vector entries plus their `naked` stubs (each loads the exception kind, the LR-adjusted faulting PC, and DFSR/DFAR or IFSR/IFAR, then branches to a common reporter). `install()` holds one block that programs VBAR and primes the ABT/UND/IRQ/FIQ banked stacks; `trigger_test_fault()` holds one deliberately-unsound read behind the `arm-fault-test` feature, which is the ARM twin of the x86 A14/A15/C2 adversarial fault tests - a fault path never observed firing is not evidence that it works. |

**ARMv7 trap worth recording: FIQ mode banks r8-r12.** The first version of `install()` stashed the
caller's CPSR in `r12`, walked through FIQ mode to set its banked stack, then restored CPSR from
`r12` - but inside FIQ that register name refers to a different physical register holding garbage, so
the restore loaded a nonsense mode and reset the CPU. The symptom was oblique (the boot banner
printing twice, and VBAR reading back as `0x00000000` instead of the table address). The fix carries
nothing across a mode switch: VBAR is programmed first while still in SVC, and the walk ends by
naming SVC explicitly rather than restoring a saved value.

## 2026-07-16 - SEC-1 / SEC-18 security fixes (feat/hardening)

Two HIGH findings from the security audit (`docs/security-audit.md`), both fixed with `// SAFETY:`-
commented blocks in the permitted `arch/` layer (no §18.5 amendment needed):

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/boot.rs` | 104 -> 107 (+3) | **APIC-timer calibration:** `read_apic` (a volatile MMIO read, the counterpart of the existing `write_apic`) and `pit_calibrate_apic_ticks_per_10ms` (an `unsafe fn` measuring the LAPIC timer against a PIT-gated 50 ms window, mirroring the proven TSC calibration). Needed because the periodic period is `init_count * divisor / f_apic` and `f_apic` is machine-dependent: the old hardcoded count gave ~100 ms on QEMU but ~1 s on the T630, 100x the intended 10 ms quantum. Same PIT ports and stuck-hardware bail-out as the TSC path; the timer LVT is masked during the measurement so it cannot deliver an interrupt. |
| `arch/x86_64/boot.rs` | 100 -> 104 (+4) | **Phase 2a (tickless idle, `docs/power.md` §14):** two safe wrappers, `rearm_idle_timer` (arm the TSC-Deadline at `IDLE_QUANTUM_MULT` quanta, ~1 s, so an idle AP wakes ~100x less often) and `rearm_quantum_timer` (restore the ~10 ms preemption quantum on an idle wake). Each wraps one `arm_tsc_deadline_now` / `rearm_tsc_deadline` call in a SAFETY-commented block, guarded by `TSC_DEADLINE_MODE`. Deliberately **safe `fn`s in the arch layer** so the neutral `scheduler.rs` calls them without `unsafe` - §18.5's rule that new `unsafe` lives in a permitted layer rather than growing a grandfathered file (scheduler.rs stays at its floor of 37). Each helper handles BOTH timer modes, so each has two SAFETY-commented blocks: the TSC-Deadline arm, and a LAPIC `APIC_TIMER_INIT` write for the periodic path (the T630 runs periodic, where the hardware auto-reloads and the initial count is the only way to slow the tick). |
| `arch/x86_64/boot.rs` | 98 → 100 (+2) | **SEC-18:** new `broadcast_nmi_all_but_self` (a `pub unsafe fn` + one `unsafe` ICR-write block) so the panic path stops every core, not just the caller. Models the sibling `broadcast_ipi_all_but_self`; NMI delivery mode (ICR bits 10:8 = 0b100) reaches a core even while it spins IF=0 on a lock. `idt[2]` is also repointed to `exception_halt` (a same-file IDT re-wire, no new `unsafe`). |
| `arch/x86_64/mod.rs` | 35 → 36 (+1) | **SEC-18:** `halt_all_cores` now calls `boot::broadcast_nmi_all_but_self()` before its `cli`+`hlt`, so a panic on one core halts the whole machine (§6.2 / §19). The +1 is that `unsafe { boot::... }` call block. |

**SEC-1** (the freed-CR3 UAF fix in `task/scheduler.rs`) adds **0** here: its Dekker-handshake edits to
`yield_current` / `block_and_reschedule` live inside those functions' pre-existing `unsafe` blocks.

## 2026-07-12 - userspace audit M8: probe made `unsafe`-free; `unsafe_check.py` now scans `services/`

`probe` (the §22 adversarial/fuzz/chaos test harness) held raw-SYSCALL `asm!` plus deliberate ring-3
faults (null read, non-canonical read, divide-by-zero) - `unsafe` in a userspace service, forbidden by
§18.2, and INVISIBLE because `unsafe_check.py` scanned only `kernel/src/`. Both gaps closed:

- The `unsafe` moved to a new **audited SDK module `sdk/rust/src/adversarial.rs`** (§18.1 amendment):
  safe `fuzz_syscall` (wraps the ABI `raw_syscall`; the kernel validates every fuzzed call) and safe
  `fault_null_read` / `fault_noncanonical_read` / `fault_divide_by_zero` (the deliberate faults, each
  SAFETY-commented). `probe` calls these safe wrappers and is now `unsafe`-free.
- `scripts/unsafe_check.py` now also scans **`services/`** and FAILS on any service `unsafe` line -
  mechanically enforcing §18.2 and catching any future regression (the M8 blind spot). As a bonus,
  `fuzz_syscall` uses the SDK's `ud2` trap, removing probe's old raw `syscall` instruction (a latent
  AMD-GX-420GI stall hazard).

Verified: `osdev test adv` 15/0 (A10 fuzz, A14 faults, A15 bad-ptr all pass through the SDK wrappers);
`unsafe_check.py` passes with `services/` scanned. Kernel inventory unchanged (471 lines).

## 2026-07-11 - core count fully dynamic (MAX_CORES ceiling removed)

Every remaining fixed `[_; MAX_CORES]` per-core structure became a boot arena sized to the machine's
real core count, and the `MAX_CORES` sanity ceiling was deleted. All changes are in the permitted
`arch/`/`smp/` layers, each block carrying a `// SAFETY:` comment.

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/mod.rs` | 35 → 33 (-2) | The fixed `AP_ID_BUF: [u32; MAX_CORES-1]` staging buffer (2 single-threaded-boot `unsafe` writes) is gone: `start_all_aps` already walks Limine's live `cpus()` slice directly, and the new `ap_count()` counts it on demand, so no AP list is staged. Net -2. |
| `arch/x86_64/syscall_entry.rs` | 14 → 15 (+1) | `PER_CORE_SYSCALL` moved from a `[_; MAX_CORES]` `.data` array to a `PerCoreMut` boot arena, with a `BSP_SYSCALL` bootstrap slot for the pre-arena window (the BSP sets its syscall GS in `init_bsp`, before the allocator). The +1 is `syscall_slot`'s `addr_of_mut!(BSP_SYSCALL)` fallback; the arena's own `unsafe` lives in `smp/percpu.rs`. |
| `smp/ipi.rs` | 23 → 25 (+2) | The TLB-shootdown ack bitmask was a fixed `PerCore<[AtomicU64; MAX_WORDS]>` (`MAX_WORDS = ceil(MAX_CORES/64)`); it is now two FLAT `num_cores * ceil(num_cores/64)` `AtomicU64` arenas (ACK + EXPECTED), so the per-initiator mask WIDTH scales with the real core count. The +2 is the `ack_word`/`exp_word` accessors (`&*base.add(initiator*wpc + word)`); the arenas are carved by `percpu::alloc_atomic_u64_slice`. |
| `smp/percpu.rs` | 6 → 8 (+2) | New `alloc_atomic_u64_slice(n, init)` - carves a flat `[AtomicU64; n]` from the frame allocator (2 blocks: `ptr::write` init loop + `from_raw_parts`) for the dynamic-width shootdown masks, which no fixed `PerCore<[_; K]>` can size. Plus `PerCoreMut::initialised()` (safe). |

Net across the four: +3. `MAX_CORES` is deleted entirely - nothing is a fixed per-core array. The only
ceiling left is a genuine hardware one: the xAPIC IPI destination field is 8-bit, so a core with LAPIC
id > 255 is excluded LOUDLY (§26.7) until the APIC layer gains x2APIC. Validated: identity 24/24, adv
15/15, QEMU boot 1..128 cores + a 72-core 2-word-shootdown restart, arenas carve for 260 cores.

---

## 2026-07-11 - per-core user-copy arenas (RAM-sized, not [_; MAX_CORES])

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/syscall_entry.rs` | 15 → 14 (-1) | The V1 per-core user-copy state (`USER_COPY_ACTIVE`, and the 1 MiB `USER_READ_SCRATCH` = `[[u8; 4096]; MAX_CORES]`) moved from fixed `[_; MAX_CORES]` statics to boot-sized `PerCore`/`PerCoreMut` arenas (sized to the cores Limine reported, §26.6.1) - so per-core memory scales to the machine, not a 256-core ceiling. The count DROPS by one: `read_user_bytes`'s `addr_of_mut!` on the static scratch became the safe `PerCoreMut::as_mut_ptr` accessor (all the arena's `unsafe` lives in `smp/percpu.rs`). The two `copy_nonoverlapping` + one `from_raw_parts` blocks are unchanged. Removes ~1 MiB of fixed `.bss`. Boot-validated across QEMU -smp 1..16 + identity 24/24 + adv 15/15. |

---

## 2026-07-11 - dynamic (RAM-sized) frame bitmap

| File | Change | Why |
|------|--------|-----|
| `memory/allocator.rs` | 37 → 44 (+7) | The frame bitmaps are no longer fixed static `[u8; N]` arrays; they are sized to the machine's actual RAM at boot and carved from RAM, reached via the HHDM. The +7 is the raw-pointer machinery that replaces the (safe-indexed) static arrays: the `bitmap()` / `kpt()` slice accessors (`slice::from_raw_parts_mut` over `BITMAP_PTR`/`KPT_PTR`, 2 blocks), and in `init_from_map` the pointer publish + `KPT_PTR = BITMAP_PTR.add(bitmap_len)` + `write_bytes` zeroing of the carved region + the `hhdm==0` guard. Each carries a `// SAFETY:` note; the region is HHDM-mapped RAM reserved before any alloc and only ever touched under `ALLOC_LOCKED`. Permitted memory layer. Net effect: 64 MiB of fixed `.bss` (at the 1 TiB static cap) replaced by a bitmap of `RAM / 16 KiB` (e.g. 14 KiB on a 256 MiB box, 4 MiB on 64 GiB), sized dynamically - validated across 256 MiB..64 GiB in QEMU + identity 24/24. |

---

## 2026-07-14 - aarch64 Phase 0: isolate arch asm behind `arch::imp` primitives

The arch-boundary seal (docs/aarch64.md) moved every inline-asm operation out of the arch-NEUTRAL layers
and behind `arch::imp` primitives, so `unsafe` asm consolidated INTO the permitted arch layer and OUT of
the neutral files. New primitives (each one `// SAFETY:`-commented): `page_tables::{read_page_table_base,
write_page_table_base, invalidate_tlb_page}`, `interrupts::{local_irq_save, local_irq_restore}`,
`mod::switch_to_boot_stack`.

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/page_tables.rs` | 41 → 46 (+5) | CR3 read/write + `invlpg` primitives (the MMU-base + TLB seam). |
| `arch/x86_64/mod.rs` | 33 → 35 (+2) | `switch_to_boot_stack` (the boot stack-pointer seam; `#[inline(always)]`). |
| `arch/x86_64/interrupts.rs` | 21 → 22 (+1) | `local_irq_save` (the irq-save half; `local_irq_restore` calls `enable_interrupts`). |
| `memory/allocator.rs` | 44 → 43 (-1) | CR3-read asm replaced by `arch::imp::read_page_table_base`. |
| `smp/ipi.rs` | 25 → 23 (-2) | CR3 reload + `invlpg` + rflags save/restore replaced by `arch::imp` primitives. |
| `smp/spinlock.rs` | 9 → 5 (-4) | irq-save/restore asm replaced by `arch::imp::local_irq_save/restore` (no-op stub in the host lib). |

Follow-on same day - **IPI-send extraction**: `smp/ipi.rs` held the last APIC MMIO in a permitted-but-not-arch layer (the ICR programming for `send_ipi` + the shootdown broadcast). Moved to `arch/x86_64/boot.rs` as `send_ipi_to_lapic(lapic_id, vector)` + `broadcast_ipi_all_but_self(vector)` (+ `apic_wait_icr_idle`); `smp/ipi.rs` now resolves core->LAPIC and holds the neutral shootdown PROTOCOL only, calling the arch seam for the send. `smp/ipi.rs` 23 -> 17 (-6, incl. the removed `read/write_apic_reg` helpers + ICR consts); `arch/x86_64/boot.rs` 92 -> 98 (+6). `smp/ipi.rs` is now APIC-MMIO-free; arch owns ALL hardware MMIO. Identity 24/0 (9A cross-core IPC + shootdown exercise the moved paths).

Net: neutral-layer asm now ZERO (enforced by `scripts/arch_boundary_check.py`, CI-wired); the arch
layer is the sole home of `unsafe` asm, as §18.1 intends. `task/scheduler.rs` + `main.rs` asm was
removed too but their `unsafe` blocks (other ops) stayed, so their counts are unchanged. Identity 24/0.

---

## 2026-07-13 - kernel-audit-3 fix: spurious-interrupt stub (K3)

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/boot.rs` | 90 → 92 (+2) | **K3 (kernel-audit-3): dedicated APIC spurious-interrupt handler.** The LAPIC spurious vector 0xFF was routed to the default `exception_halt`, so a spurious IRQ (a normal, rare hardware event the SDM says to ignore-and-return) would wedge the whole machine. New `spurious_stub` (`#[unsafe(naked)] unsafe extern "C" fn` + `naked_asm!("iretq")`) gives 0xFF a return-without-EOI handler - the exact naked-stub pattern of the sibling `ipi_wake_stub`, no register save / no swapgs needed because it touches nothing. The +2 is the `#[unsafe(naked)]` attribute + the `unsafe extern "C" fn`. Permitted arch layer; carries a doc comment explaining soundness. |

---

## 2026-07-11 - kernel-audit fixes: user-copy fault guard (V1) + exception-handler backfill

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/syscall_entry.rs` | 13 → 15 (+2) | **V1 (kernel-audit-2): user-copy fault guard.** `read_user_bytes` no longer returns a borrowed raw-user slice; it copies the user bytes into a per-core kernel scratch under a `USER_COPY_ACTIVE` guard and returns a slice into the SCRATCH (so no caller ever dereferences raw user memory). The net +2 is: `read_user_bytes` now has 3 blocks (`addr_of_mut!` on the per-core scratch, `copy_nonoverlapping` for the guarded copy, `from_raw_parts` for the returned slice) vs 1 before; `write_user_bytes` keeps its 1 `copy_nonoverlapping` block. Each has a `// SAFETY:` comment. The guard makes a fault on a range-valid-but-unmapped user pointer recoverable (pf_handler kills the caller) instead of a whole-machine halt. Permitted arch layer. |
| `arch/x86_64/boot.rs` | 84 → 90 (+6) | **Backfill (not this change):** reconciles the C1/C2 exception-kill handlers added earlier this session by `af74086` (`gpf_stub` / `gpf_handler` / `exc_stub_noec` / `exc_stub_ec` / `exc_dispatch` - the ring-3 CPU-exception discriminator), which did not bump this table. All in the permitted arch layer, each block carries a `// SAFETY:` comment. **V1's own pf_handler change adds 0 here** - its user-copy-fault branch is safe code (calls to `current_core_id` / `user_copy_active` / `clear_user_copy_active`) inside the existing print `unsafe` block. |

---

## 2026-07-10 - fast fbcon blits for the 4K Wyse 5070 + drift reconcile

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 3 → 5 (+2) | Fast blit path for a dense (4K) panel. `fill_rect` writes a solid rectangle as contiguous per-row runs; `draw_glyph`'s fast 32bpp path writes each glyph output row as one contiguous run of aligned `u32` stores. The old per-pixel byte loop crawled repainting ~6.6M pixels/scroll on the Wyse's 3840x2160 panel. Both bounds-check the whole rect/cell ONCE against the reported geometry (cols/rows are sized so cells fit) then write the run unchecked, so write-combining coalesces the stores - the same raw-framebuffer-write pattern as the existing `put_pixel`/`clear`, in the permitted arch layer, each with a `// SAFETY:` comment. There is no safe route: writing Limine's linear framebuffer is a raw-pointer store, and a bounds-checked `&mut [u32]` would defeat the purpose (a compare per pixel is the very overhead removed). |
| `arch/x86_64/boot.rs` | 80 → 84 (+4) | **Reconcile only:** pre-existing drift accumulated since 2026-06-08 by the feat/networking bring-up (H1 IOMMU / NIC / PCI / AHCI, merged to main) that did not update this audit. All in the permitted arch layer; count corrected here, per-block detail is a backfill owed by that work. |
| `arch/x86_64/mod.rs` | 34 → 35 (+1) | **Reconcile only:** pre-existing drift from feat/networking (merged to main); permitted arch layer, count corrected, per-block detail backfill pending. |

## 2026-06-08 - fbcon scroll without VRAM read-back

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 4 → 3 (−1) | `scroll` no longer `core::ptr::copy`s the framebuffer up in place (which *read* uncached/WC VRAM - ~130 ms/line on the T630, the fbcon perf trap behind the "40× respawn"). It now shifts a RAM char-grid shadow and repaints from it - write-only via `draw_glyph`/`put_pixel` - so the block is gone. |

Reduction only; locks in the lower count. The three remaining blocks (`clear`,
`put_pixel`, `wc_flush`) keep their `// SAFETY:` comments. Hardware-verified
(T630): pixel-correct after thousands of scrolls; spawn 0.906 s → 9.9 ms.

> **Note.** This same day also reconciled 3 files the earlier H4b/H4 hardening
> merges left unaccounted (`page_tables.rs 25→35` permitted; `main.rs` and
> `task/mod.rs` held at their floors 2 and 7 by the clip) - see the entry below.

---

## 2026-06-08 - H4 hardening reconcile, **grandfathered floors held (no amendment)**

The W^X-remap (H4a/H4b) and kstack-guard (H4) work that merged earlier this session
added `unsafe` (all `// SAFETY:`-commented in source) without updating this audit. It
*briefly* raised two grandfathered floors; that was then **clipped back** so the
grandfathered counts return to their long-standing floors and **no §18 amendment is
needed**. The hardening's page-table `unsafe` now lives in the permitted arch layer,
where §18.1 says page-table manipulation belongs.

| File | Net | Layer | What |
|------|-----|-------|------|
| `arch/x86_64/page_tables.rs` | 25 → 35 (+10) | permitted | `entry_for_va`/`walk` PTE-walk + `unmap_active_4k` + `harden_hhdm_nx` (now a safe `fn`) + new `unmap_4k_strided` (the kstack guard-unmap loop, moved here from `task/`). Permitted-layer growth, allowed with SAFETY comments + this entry. |
| `main.rs` | 2 → 2 (no change) | grandfathered | `install_kstack_guards` / `harden_hhdm_nx` are now **safe `fn`s** (their preconditions are boot-ordering, not UB - same shape as `memory::init`/`smp::init`), so the call sites need no `unsafe`. |
| `task/mod.rs` | 7 → 7 (no change) | grandfathered | `install_kstack_guards` is now a safe `fn` whose guard-unmap delegates to `page_tables::unmap_4k_strided` (arch); the static-pool-address `unsafe` is centralised in `kstack_pool_base()` and reused by `free_kstack`, so the net count is unchanged. |

**Why this is better than amending (the clip).** `unsafe fn` is for memory-safety
preconditions whose violation is *UB*; `harden_hhdm_nx` / `install_kstack_guards` have
only *boot-ordering* preconditions (calling them out of order wedges boot - a liveness
bug, not UB), exactly like the already-safe `memory::init` / `smp::init`. Marking them
safe is both more honest and removes the call-site `unsafe`. The genuinely-unsafe work
(CR3 reads, PTE writes, the page unmap) stays in `unsafe {}` blocks **inside the
permitted arch layer** (§18.1). Net: the security hardening landed with **zero**
grandfathered growth. Hardware-verified on the T630 (guard pages install; W^X holds).

---

## 2026-06-04 - idle-halt (cool when idle) + introspection holds-check reconcile

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/interrupts.rs` | 12 → 13 (+1) | `wait_for_interrupt` gains a `sti; hlt` branch so ARAT-capable cores halt (run cool) instead of spinning; the no-ARAT branch keeps the legacy `sti`-only spin. |
| `arch/x86_64/boot.rs` | 79 → 81 (+2) | `cpuid_arat_supported` (`unsafe fn` + `__cpuid(6)`) - detects whether the LAPIC timer survives a C-state, gating the halt. |
| `task/scheduler.rs` | 36 → 37 (+1, grandfathered) | reconciles `current_task_holds_resource` - the §3.1 introspection holds-check (mirrors the existing grandfathered `current_task_lookup_cap`: reads `TASK_CAP[cur].assume_init_ref()`). Added with the introspection gate; the audit count was not bumped then - corrected here. A single read-only line for a security gate, same pattern as the lines already grandfathered in this file. |

All blocks carry `// SAFETY:` comments. The `hlt` is ARAT/TSC-Deadline-gated, so on
hardware without an always-running timer it never executes (no regression).

---

## 2026-06-03 - USB/xHCI stack (boot-verified, T630)

Branch `feat/usb-keyboard`. The userspace USB keyboard stack (§12) added unsafe
in the permitted arch + memory layers (the driver *service* itself is unsafe-free
behind the SDK's audited `Mmio`/`Dma` wrappers - §18.1).

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/pci.rs` | **new, 5 lines** | PCI config mechanism #1 port I/O (`outl`/`inl` + `config_read32`) to locate the xHCI controller. |
| `arch/x86_64/mod.rs` | 33 → 34 (+1) | `console_push_byte` pushes a USB-decoded key into the COM1 RX ring (`uart_rx_push`) so keystrokes reach the shell's `ConsoleRead`. |
| `memory/allocator.rs` | 29 → 32 (+3) | `alloc_contiguous(n)` - bitmap scan for a physically-contiguous run, for the driver's DMA arena. |

All blocks carry `// SAFETY:` comments in source. SDK `mmio.rs`/`dma.rs` unsafe
lives outside `kernel/src/` (the §18.1-amended SDK hardware/ABI layer) and is not
counted by `scripts/unsafe_check.py`, which scans `kernel/src/` only.

---

## 2026-05-31 - static-analysis + unsafe-audit pass (boot-verified, T630)

Full write-up: `milestones/testing/static-analysis-audit.md`. Branch
`verify/static-analysis-unsafe-audit`, commit `d276566`.

| Area | Result |
|------|--------|
| Policy violation | **Fixed** - `unsafe` removed from `ipc/` (§18.1); moved to `SpinLock::ZEROED` in `smp/spinlock.rs`. |
| Safety / correctness lints | **0** - 11 unnecessary `unsafe`, 11 `static mut` refs (→ `addr_of!`), 14 fn-item→int casts, 6 no-op `mem::forget`. |
| Cruft removed | orphaned `page_fault_handler` + `INTERRUPTED_*` statics. |
| Inventory | reconciled below - 302 lines / 23 files, passes clean; `task/scheduler.rs` 37 → 36 (under floor). |
| Kernel warnings | 104 → 57 (rest intentional unwired architecture). |
| Hardware | boots clean on T630, cross-core ping/pong to 83k+ msgs, zero `#PF`/panic. |

---

## Policy (§18)

`unsafe` is permitted only in:

| Permitted layer | Path |
|---|---|
| Architecture | `kernel/src/arch/` |
| Memory | `kernel/src/memory/` |
| Capability table | `kernel/src/capability/` |
| SMP | `kernel/src/smp/` |

**All other locations are outside policy.** The files marked `grandfathered` in
the table below contain unsafe that pre-dates this audit. Their counts are frozen:
they may decrease (fix welcome) but may not increase. New unsafe in those files
requires a policy amendment in `CLAUDE.md §18` before CI will accept it.

When you add an `unsafe` block anywhere:

1. Add `// SAFETY: <argument>` on the line immediately above it in the source.
2. Increase the count for that file in the inventory table below.
3. Add a SAFETY argument entry under that file in the **Entries** section.
4. Both changes must land in the same commit; CI checks the count.

---

## Inventory

Counts are non-comment lines containing the `unsafe` keyword.
CI script: `scripts/unsafe_check.py` - parses the table between the markers.

<!-- unsafe-inventory-start -->
| File (kernel/src/) | Count | Layer |
|---|---|---|
| arch/aarch64/mod.rs | 68 | permitted |
| arch/aarch64/sched_user.rs | 4 | permitted |
| arch/aarch64/uart_rx.rs | 3 | permitted |
| arch/aarch64/exceptions.rs | 15 | permitted |
| arch/aarch64/uaccess.rs | 7 | permitted |
| arch/aarch64/context.rs | 9 | permitted |
| arch/aarch64/sched_demo.rs | 5 | permitted |
| arch/aarch64/ctxdemo.rs | 7 | permitted |
| arch/aarch64/gic.rs | 7 | permitted |
| arch/aarch64/timer.rs | 5 | permitted |
| arch/aarch64/mmu.rs | 23 | permitted |
| arch/aarch64/ptables.rs | 21 | permitted |
| arch/aarch64/usermode.rs | 16 | permitted |
| arch/aarch64/mailbox.rs | 4 | permitted |
| arch/aarch64/memmap.rs | 8 | permitted |
| arch/aarch64/video.rs | 2 | permitted |
| arch/aarch64/genet.rs | 1 | permitted |
| arch/aarch64/pcie.rs | 4 | permitted |
| arch/aarch64/smp_boot.rs | 9 | permitted |
| arch/arm/exceptions.rs | 24 | permitted |
| arch/arm/context.rs | 6 | permitted |
| arch/arm/context_switch.rs | 13 | permitted |
| arch/arm/dtb.rs | 6 | permitted |
| arch/arm/irq.rs | 16 | permitted |
| arch/arm/meminit.rs | 4 | permitted |
| arch/arm/mmu.rs | 8 | permitted |
| arch/arm/video.rs | 17 | permitted |
| arch/arm/fbcon.rs | 1 | permitted |
| arch/arm/dwc2.rs | 34 | permitted |
| arch/arm/page_tables.rs | 31 | permitted |
| arch/arm/sched_demo.rs | 6 | permitted |
| arch/arm/sched_ipc.rs | 9 | permitted |
| arch/arm/spawn.rs | 4 | permitted |
| arch/arm/syscall.rs | 5 | permitted |
| arch/arm/usermode.rs | 15 | permitted |
| arch/arm/timer.rs | 4 | permitted |
| arch/arm/mod.rs | 45 | permitted |
| arch/loongarch64/mod.rs | 25 | permitted |
| arch/riscv32/mod.rs | 25 | permitted |
| arch/riscv64/mod.rs | 25 | permitted |
| arch/s390x/mod.rs | 20 | permitted |
| arch/x86_64/ap_boot.rs | 2 | permitted |
| arch/x86_64/boot.rs | 107 | permitted |
| arch/x86_64/context_switch.rs | 11 | permitted |
| arch/x86_64/fb.rs | 2 | permitted |
| arch/x86_64/interrupts.rs | 22 | permitted |
| arch/x86_64/ioapic.rs | 8 | permitted |
| arch/x86_64/iommu.rs | 74 | permitted |
| arch/x86_64/mod.rs | 36 | permitted |
| arch/x86_64/page_tables.rs | 51 | permitted |
| arch/x86_64/pci.rs | 20 | permitted |
| arch/x86_64/rtc.rs | 1 | permitted |
| arch/x86_64/syscall_entry.rs | 15 | permitted |
| capability/table.rs | 7 | permitted |
| memory/allocator.rs | 44 | permitted |
| memory/frame.rs | 1 | permitted |
| memory/mod.rs | 1 | permitted |
| memory/page.rs | 1 | permitted |
| smp/ipi.rs | 17 | permitted |
| smp/mod.rs | 1 | permitted |
| smp/percpu.rs | 8 | permitted |
| smp/placement.rs | 1 | permitted |
| smp/spinlock.rs | 5 | permitted |
| interrupt/route.rs | 1 | grandfathered |
| loader.rs | 2 | grandfathered |
| main.rs | 2 | grandfathered |
| syscall/dispatch.rs | 2 | grandfathered |
| task/mod.rs | 7 | grandfathered |
| task/scheduler.rs | 37 | grandfathered |
<!-- unsafe-inventory-end -->

**Permitted total:** 394 lines across 22 files  
**Grandfathered total:** 53 lines across 6 files  
**Grand total:** 447 lines across 28 files

> **2026-06-28** (branch `hardening/dma-reserve-pool`). **Audit reconciliation** - three permitted-layer
> files drifted (each line already carrying a `// SAFETY:` comment; the counts just weren't bumped as the
> `arch/x86_64/pci.rs` 19 → 20 (C8-1, 2026-08-14): one `rdtsc()` helper, added when the two BIOS-handoff
> waits stopped counting register reads and started bounding themselves by a clock (Commandment VIII).
> A helper rather than four inline blocks: the same idea is tested twice in each of two places, and
> repeating `unsafe` would have grown the audited surface fourfold for one read of a CPU counter.
> `arch/x86_64/pci.rs` 17 → 19: `clear_bus_master` + `set_bus_master`, the PCI bus-master
> quiesce on DMA-driver kill/spawn that cures the max-carnage DMA-after-free (commit `ffe1a0f`).
> `memory/allocator.rs` 32 → 37: one from the page-table reclaim guard (`phys_in_ram`, commit `b9dbc4c`)
> and four from the DMA permanent-reserve net (§12) added on this branch - `alloc_dma_arena` (the reserving
> allocator + its public wrapper + the table-full undo) so a driver's DMA arena is never recycled into a
> page table. `smp/spinlock.rs` 7 → 9: a second `without_interrupts` guard from the per-core shootdown work.
> New grand total: 447 / 28 files (also corrects the prose totals, which had drifted ~5 low vs the inventory sum).

> **2026-06-22** (branch `fix/unsafe-audit-reconcile`). **Audit reconciliation** - caught up four
> drifted files and shrank one back under its floor. Permitted-layer count catch-ups (all `arch/`,
> each line already carrying a SAFETY comment - no policy issue, the counts just weren't bumped as the
> work landed): `arch/x86_64/interrupts.rs` 13 → 21 (USB MSI ISR plumbing), `arch/x86_64/pci.rs`
> 15 → 17 (MSI-X table mapping), and the previously-unlisted file `arch/x86_64/ioapic.rs` (+8, IOAPIC
> MMIO register reads/writes for legacy-INTx routing). `smp/spinlock.rs` 5 → 7 (the `without_interrupts`
> cli/sti added for the kstack-lock irqsafe fix). **`task/scheduler.rs` 40 → 37 - back at floor, NO
> §18.5 amendment:** the 3 file-as-capability (§7.10) accessors that had drifted it over -
> `current_task_endpoint`, `set_last_recv_badge`, `take_last_recv_badge` - were converted from
> `static mut` (`TASK_ENDPOINT`, `TASK_LAST_BADGE`) to `AtomicU64`, making them `unsafe`-free. The
> grandfathered floor stays 37 and there are still **no** grandfathered-floor amendments. New grand
> total: 433 / 28 files.

> **2026-06-13** (branch `feat/persistence`). **ATA PIO / `hw_pio` retired** - the
> AHCI (MMIO+DMA) backend replaced ATA PIO (the T630's SSD is AHCI-only). Reverts the
> 2026-06-12 addition below: `arch/x86_64/mod.rs` 38 → 34 (the `port_in8/16`,
> `port_out8/16` wrappers removed; `inb`/`outb` stay - used by serial + reboot), and
> `capability/hw_pio.rs` deleted (−3). Back to 413/27. The `PortRead`/`PortWrite`
> syscalls and the SDK `pio.rs` (not kernel-audited) are gone too.

> **2026-06-12** (branch `feat/persistence`). Persistence Phase 1 (ATA PIO block
> driver, docs/persistence.md §5). `arch/x86_64/mod.rs` +4 (permitted): safe
> public port-I/O wrappers `port_in8/16` + `port_out8/16` (the `in`/`out` asm,
> isolated in the arch layer; callers validate the port first). New file
> `capability/hw_pio.rs` +3 (permitted): the per-task `hw_pio` grant store
> (`set`/`clear`/`allowed`) - placed in the capability layer **on purpose**, so
> the per-task port-range state does not grow the grandfathered `unsafe` floor in
> `task/` (§18.5). `task/scheduler.rs` and `syscall/dispatch.rs` gained **no**
> `unsafe` (they call the safe wrappers / the capability-layer functions).

> **2026-06-10** (branch `feat/iommu-dma-confinement`). New file `arch/x86_64/iommu.rs`
> (+60, permitted): the H1 AMD-Vi IOMMU work. Phase 0 (+18) is ACPI-table reads
> (RSDP → RSDT/XSDT → IVRS) through the HHDM. Phase 1 (+42) is the IOMMU control
> interface and translation setup: uncached MMIO register read/write, device-table
> /command-buffer/event-log allocation and DTE writes, the 4-level I/O page-table
> builder/translator/free, and command-buffer invalidation. Every block carries a
> `// SAFETY:` argument that the target is a kernel-mapped IOMMU structure (MMIO
> window, device table, command buffer, or I/O page table) and the access is in
> bounds. All hardware `unsafe` is contained here behind the safe wrapper
> `confine_device()`; `task/mod.rs` calls it without any new `unsafe` (its
> grandfathered floor of 7 is unchanged). See the `arch/x86_64/iommu.rs` entry below.

> **Reconciled 2026-05-31** (branch `verify/static-analysis-unsafe-audit`). The
> permitted-layer growth since the prior baseline is from the AMD GX-420GI ring-3 /
> TSC-Deadline-APIC / COM1 work that landed on `main` (boot.rs, mod.rs, interrupts.rs,
> ipi.rs, allocator.rs). `smp/spinlock.rs` +1 is the new `ZEROED` const (below).
> Reductions: the static-analysis pass removed unnecessary `unsafe` blocks
> (ap_boot, boot, mod, scheduler) and the orphaned `page_fault_handler` /
> `INTERRUPTED_*` diagnostics (interrupts.rs net still up from the AMD work).
> **`task/scheduler.rs` is back to 36** - under its grandfathered floor again.

---

## Entries

Each entry documents WHY an unsafe block is sound. Entries are grouped by file.
Files with thorough existing `// SAFETY:` comments in source reference them here.
Files lacking source comments are noted with `(SAFETY comment missing in source)`.

New entries must be added in the same commit as the unsafe block they cover.

---

### arch/x86_64/ap_boot.rs

Unsafe in this file: AP trampoline entry, AP boot identity mapping, and calling
`ap_main` after the long-mode switch. All three are sound because the trampoline
runs before any Rust invariants apply; the stack is valid; identity mapping holds
for the trampoline duration and is torn down by the kernel immediately after.

---

### arch/x86_64/boot.rs

Largest unsafe surface in the kernel. Covers: BSP init (GDT/IDT/TSS per core),
APIC MMIO mapping and register writes, serial I/O port access, TSS RSP0 reads
and writes, paging init, and IPI delivery. All operations are sound because
they target fixed hardware addresses verified against the Limine memory map, or
operate on per-core structures indexed by a valid `core_id` bounded by
`MAX_CORES`. APIC MMIO is mapped once before any AP comes up.

One additional `unsafe {}` block (count +1): `write_apic(apic_virt, APIC_TPR, 0x00)`
in `init_local_apic` - zeroes the Task Priority Register so all interrupt
vector classes (including `WAKE_RECEIVER` at 0xF0) are accepted. Sound because
`apic_virt` is established by the preceding `map_in_active_tables` call within
the same function; `APIC_TPR` offset is within the mapped 4 KiB MMIO page.
`// SAFETY:` comment present in source.

---

### arch/x86_64/context_switch.rs

Stack construction for new kernel and user tasks. `new_kernel` and `new_user`
write a synthetic initial register frame to a freshly allocated kernel stack
pointer. Sound because the stack buffer is owned exclusively by the new task
and not yet visible to the scheduler.

---

### arch/x86_64/fb.rs

Framebuffer text console (Phase 1 boot output, §11.4). Five blocks; four write
to Limine's linear framebuffer at `base + y*pitch + x*bpp`:
- `clear`: `write_bytes(base, 0, height*pitch)` - fills the whole buffer.
- `put_pixel`: writes `bpp` bytes (one aligned `u32` store on the 32bpp fast path) at a
  bounds-checked offset (`x<width`, `y<height`).
- `fill_rect`: fills a `w x h` rectangle clamped to `width`/`height`, writing each row as a
  contiguous run (aligned `u32` stores on the 32bpp path). Sound: the clamped rect stays inside the
  mapped `height*pitch` region; `x*bpp`/`pitch` are 4-aligned on the 32bpp path.
- `draw_glyph` (fast 32bpp path): writes each glyph output row as one contiguous run of `cw` aligned
  `u32` stores. Sound: it first checks the whole cell `[x0,x0+cw) x [y0,y0+chh)` lies inside the
  framebuffer (cols/rows are sized so cells fit; otherwise it falls back to the checked `fill_rect`),
  so the unchecked run stays within `height*pitch`, and `x0*4`/`pitch` are 4-aligned.

Sound because the framebuffer is the region Limine mapped and sized
(`height*pitch` bytes), it lives in the higher half (PML4 256-511) that every
address space inherits via `PageTable::new`, so it is valid for writes for the
system lifetime; every offset is bounds-checked against the reported geometry.

`scroll` previously held a fourth block - an in-place `copy`/`write_bytes` that
shifted the framebuffer up one glyph row. That `copy` *read the framebuffer back*
(uncached/WC VRAM, ~130 ms/line on the T630); it was replaced by a RAM char-grid
shadow that scroll shifts and repaints from, leaving `scroll` entirely safe
(write-only via `draw_glyph` → `put_pixel`). Net **4 → 3** (2026-06-08).

The remaining `wc_flush` block is a single `SFENCE` instruction. The framebuffer
is mapped write-combining (Limine HHDM default), so the FB lock's atomic release
does not order the WC store buffer - a scroll's pixel stores on one core could
flush after the next line's first glyph drawn on another core, erasing it. Each
`put_byte`/`put_bytes` issues `SFENCE` before releasing the lock so its WC stores
are globally visible in order. Sound because `SFENCE` only orders stores and has
no memory or privilege effects.

---

### arch/x86_64/interrupts.rs

IRQ dispatch and CR2 read on page fault. Sound because the IRQ handler runs at
known IDT vector; CR2 is only read inside the page-fault handler where it is
valid.

Three additional `unsafe {}` blocks (count +3): `enable_interrupts` (STI),
`disable_interrupts` (CLI), and `wait_for_interrupt` (STI+HLT). All three are
ring-0 privileged instructions with no memory effects; the callers are
responsible for the context invariants (e.g., interrupts were disabled before
calling `wait_for_interrupt`). `// SAFETY:` comments present in source.

One additional `unsafe {}` block (count +1): `send_eoi` - writes the local APIC
EOI register via `boot::apic_send_eoi`. Sound because the APIC is mapped before
any IRQ fires and EOI register writes are idempotent with no memory-safety
implications. Exposes APIC EOI as a safe call site in `interrupt/route.rs` (§12)
without increasing the grandfathered count there.

One additional `unsafe {}` block (count +1): `fire_test_irq` - calls
`interrupt::route::deliver(irq)` after disabling interrupts and before
re-enabling them. Sound because IF=0 satisfies `deliver`'s calling convention;
the surrounding `disable_interrupts()` / `enable_interrupts()` calls are safe
arch functions; EOI inside `deliver` is idempotent outside a real hardware
interrupt. Used only by the `FIRE_IRQ` COM2 control command (§22 Tests IR1A/IR1B).

---

### arch/x86_64/ioapic.rs

IOAPIC programming for legacy-INTx interrupt routing. 8 unsafe lines, each with a SAFETY comment -
uncached MMIO access to the IOAPIC index/data window (write the register selector, then read/write the
32-bit data register) to read the IOAPIC id/version and program redirection-table entries that route a
legacy IRQ line to a CPU vector. Permitted `arch/` layer (direct hardware access, §18.1). The file was
not previously in the inventory; its count was correct in source, just unaudited - added 2026-06-22.

---

### arch/x86_64/iommu.rs

AMD-Vi IOMMU detection (H1 Phase 0). All 18 unsafe lines are raw reads of
firmware ACPI tables - the RSDP, the RSDT/XSDT, and the IVRS - through the HHDM.
The helpers `read_bytes`, `read32`, `read64` are `unsafe fn`; `detect` calls them
at every step. Each block is sound because:

- The RSDP virtual address comes from Limine's `RsdpRequest`, which points at a
  table Limine keeps mapped in the HHDM; the signature is checked before any
  further read.
- Every subsequent table is reached only through a physical pointer that lives
  inside an already-validated parent table, converted to a virtual address via
  the HHDM (`hhdm + phys`), which Limine maps for all usable + ACPI memory.
- Each read stays within the table's own length field (`sdt_len`, `ivrs_len`),
  and the IVHD walk advances by the block's self-reported length and stops on a
  zero length, so it cannot run off the end or loop forever.

Detection only - no behaviour change, no writes, no device programming. The
results are published in two atomics (`IOMMU_PRESENT`, `IOMMU_MMIO_BASE`).

**Phase 1 (translation setup), +42.** The remaining unsafe in this file programs
the IOMMU and builds translation structures. Grouped:

- `mmio_read64` / `mmio_write64` - volatile access to the IOMMU MMIO control
  registers, which `bringup` maps uncached (PCD|PWT) at their HHDM alias before
  any access. Offsets are compile-time constants within the mapped 0x4000 window.
- `setup_structures` / `write_dte` - allocate the device table (2 MiB contiguous),
  command buffer, and event log from the frame allocator, zero them through the
  HHDM, and write DTEs. All writes target the freshly-allocated, HHDM-mapped
  structures; the DTE index is a 16-bit BDF, in bounds of the 64K-entry table.
- `io_walk_or_alloc` / `io_map_page` / `io_translate` / `free_io_table` - the
  4-level AMD-Vi I/O page-table builder, read-only translator, and frame reclaim.
  Each level VA is the HHDM alias of a present/just-allocated table; indices are
  masked to 9 bits (< 512), so every read/write is in bounds of a 4 KiB table.
  `free_io_table` frees only the page-table frames (reached top-down from a root
  that `release_device` has already detached from the device), never the leaf
  arena pages.
- `invalidate_device` - writes 16-byte commands into the mapped command-buffer
  ring at the hardware tail offset (masked to the 4 KiB ring) and rings the tail
  register; serialised by `CMD_LOCK`.
- `drain_event_log` - reads decoded fault events from the mapped 4 KiB event-log
  ring (head < 0x1000) and advances the head register; bounded per call so it is
  safe to invoke from the timer-tick path (`control::process_pending`). Also
  recovers from event-log overflow (disable EvtLogEn, RW1C the status bit, reset
  head/tail, re-enable) - all writes to valid IOMMU control/status/pointer regs.
- `confine_device` / `confinement_selftest` / `release_device` - orchestrate the
  above; the raw work they do directly is zeroing a freshly-allocated page table,
  an `sfence` (no memory-safety effect, orders prior stores), and (on release)
  reverting a DTE before freeing the now-unreachable I/O page table.

`confine_device`, `release_device`, `event_log_state`, and `bringup` are the safe
entry points;
all callers outside the arch layer (e.g. `task/mod.rs`) use them without `unsafe`.
`// SAFETY:` comments present on every block.

---

### arch/x86_64/mod.rs

Serial port init and COM2 init via `outb`/`inb`, `cli`/`hlt` in the halt loop,
and the `init` call chain into `boot.rs`. Sound because serial ports are
exclusively owned by the kernel at these call sites; `cli`/`hlt` is the correct
halt sequence; all callers are within the single-threaded BSP init path.

One additional `unsafe {}` block (count +1): `console_push_byte` calls
`uart_rx_push(b)` to enqueue a USB-keyboard-decoded byte into the COM1 RX ring,
then wakes any task blocked in `ConsoleRead`. Sound because the RX ring is a
single-logical-producer buffer (the timer-ISR UART drain and the xHCI driver's
`ConsolePush` syscall both run on Core 0's serial path); the push is a bounded
ring write with head/tail wrap. `// SAFETY:` comment present in source.

---

### arch/x86_64/page_tables.rs

HHDM offset reads and writes, PTE reads and writes via `read_volatile`/
`write_volatile`, `map_in_active_tables` (reads CR3, walks and modifies the
active page table), and `reclaim_user_frames` (walks a dead task's page table
after the TLB shootdown has completed). All are sound because: HHDM offset is
written once before any AP starts; PTE access goes through the HHDM which is
valid after `set_hhdm_offset`; `map_in_active_tables` holds the frame allocator
lock for the duration; `reclaim_user_frames` is called only after TLB shootdown
acknowledgment from all cores.

Ten additional unsafe lines (count 25 → 35) from the W^X / guard-page hardening
(H4a/H4b, 2026-06-07/08):
- `entry_for_va` / `walk` / `read_entry` chain - read-only PTE walk used to probe
  a VA's mapping (PTE/large-page) for the W^X audit and the kstack-guard install.
- `unmap_active_4k(virt)` (`unsafe fn` + CR3 read + present-entry walk + clear PTE
  + `invlpg`) - marks a 4 KiB page non-present; no-ops on a large page (fails safe).
- `unmap_4k_strided(base, stride, count)` - a **safe `fn`** that unmaps the low page
  of each kstack slot via `unmap_active_4k`; the guard-unmap loop moved here from
  `task/` (§18.1 - page-table work belongs in arch) so it adds no grandfathered
  unsafe. Boot-ordering contract (BSP, before APs).
- `harden_hhdm_nx()` - a **safe `fn`** (CR3 read + HHDM subtree walk OR-ing NX into
  every present PDPT/PD/PT, then CR3 reload) that flips the HHDM `NO_EXEC`, closing the
  Limine-mapped RWX direct map (§3.12). Boot-ordering precondition (after `smp::init`),
  not UB - hence safe; the CR3/PTE work inside stays `unsafe {}`.

Six further unsafe lines (count 35 -> 41) from the `alloc_mem` reclaim-leak fix
(2026-06-23, surfaced by `chaos mem-pressure`):
- `free_phys_frame(phys)` (an `unsafe fn` + one `unsafe { free_frame }`) - frees one
  physical frame by address during task-death teardown.
- `reclaim_user_frames` now frees each leaf / PDPT / PD / PT frame INLINE via
  `free_phys_frame` (four call sites) instead of collecting into the fixed 512-entry
  `ReclaimBuffer`, whose `push` silently DROPPED - i.e. LEAKED - every frame past 512 (a
  32 MiB `alloc_mem` task leaked ~30 MiB on every kill, violating §10.5 / §26.7). The walk
  itself is unchanged; only "collect into a buffer" became "free inline". Sound for the same
  reason as the original: called only after the TLB shootdown has been acknowledged by all
  cores, so no core's page-walker can reach a freed frame.

All sound for the same reason as the rest of the file: HHDM is live, the tables are
reached via present entries rooted at the live CR3-referenced PML4, and these run
BSP-only at boot before APs execute from the affected region. `// SAFETY:` comments
and `# Safety` docs present in source for every block.

---

### arch/x86_64/pci.rs

PCI configuration-space access via legacy mechanism #1 (port `0xCF8` address /
`0xCFC` data), used once at boot to locate the xHCI USB host controller and
record its MMIO base + IRQ (§12). Five unsafe lines:
- `unsafe fn outl` / its inner `unsafe {}` block - 32-bit `out dx, eax` port write.
- `unsafe fn inl` / its inner `unsafe {}` block - 32-bit `in eax, dx` port read.
- `unsafe {}` in `config_read32` - pairs an `outl(address)` then `inl(data)`.

Sound because port I/O is ring-0 and these ports are the architecturally fixed
PCI config registers, owned exclusively by the kernel during single-threaded BSP
boot (the scan runs before any AP or task exists); the address dword is
constructed from bounded bus/dev/func/offset values with the enable bit set per
the mechanism-#1 spec. `// SAFETY:` comments present in source.

Three additional unsafe lines (+3) for the EHCI BIOS→OS handoff
(`ehci_bios_handoff`): the `unsafe {}` in `config_write32` (paired `outl(address)`
+ `outl(data)`, same discipline as `config_read32`), the `map_in_active_tables`
call mapping the EHCI MMIO page to read HCCPARAMS, and the `read_volatile` of
HCCPARAMS. Sound for the same reason - ring-0 BSP boot, architecturally fixed
ports, the MMIO page mapped uncached before the single aligned read.

Seven more (+7) for the xHCI BIOS→OS handoff (`xhci_bios_handoff`): xHCI's legacy
support lives in MMIO (not PCI config), so this maps the xHCI MMIO (16 pages,
uncached), reads HCCPARAMS1 for the xECP, then walks the MMIO extended-capability
list - `read_volatile`/`write_volatile` of USBLEGSUP (claim OS ownership, poll
for BIOS release) and USBLEGCTLSTS (disable firmware SMIs). Each access is within
the just-mapped 64 KiB MMIO window at a bounded offset (< 0x10000), during
single-threaded BSP boot. All carry `// SAFETY:` comments.

---

### arch/x86_64/rtc.rs

MC146818 CMOS real-time-clock read via the legacy index/data ports (`0x70` /
`0x71`), used to answer `InspectKernel` query 11 (wall-clock date/time) for the
shell's `date`/`time` commands (§12). One unsafe line:
- `unsafe {}` in `cmos_read` - wraps an `out dx, al` (select register) followed by
  an `in al, dx` (read its value); the two asm blocks are not `pure`, so their
  order is preserved.

Sound because port I/O is ring-0 and these are the architecturally fixed CMOS
ports; only a register number (`0x00..0x3F`) is written, and the read is
side-effect-free. The driver is read-only - it never writes CMOS - so it cannot
disturb other clock/NMI state. `// SAFETY:` comment present in source.

---

### arch/x86_64/syscall_entry.rs

Serial output helpers (`ser_putc`, `ser_puts`, `ser_hex64`) and per-core SYSCALL
MSR setup. Sound because serial helpers are guarded by the kernel's serial
spinlock; SYSCALL MSR setup runs once during per-core init before the core
enters the scheduler.

Three additional `unsafe {}` blocks (count +3): `read_user_bytes`
(`from_raw_parts`), `write_user_bytes` (`copy_nonoverlapping`), and
`read_cycle_counter` (`_rdtsc`). All three are sound because the pointer/length
pair is validated by `validate_user_ptr` before the unsafe call, ensuring the
range lies in user-space (below `USER_END`) and cannot overlap kernel memory;
`_rdtsc` is a read-only counter with no side effects. `// SAFETY:` comments
present in source.

---

### capability/table.rs

Access to `GLOBAL_RESOURCES` - a static `ResourceTable` protected by an
internal `SpinLock`. All seven unsafe calls go through the lock; the lock
ensures mutual exclusion across cores. `// SAFETY:` comments present in source.

---

### memory/allocator.rs

Frame allocator internals: bitmap manipulation, guard-page checks, allocator
init from the Limine memory map. Sound because the allocator is protected by a
`SpinLock`; bitmap indices are bounds-checked before access; guard-page ranges
are set once during init. `// SAFETY:` comments present in source for most
blocks; a small number need back-fill (see grandfathered note in §18).

Three additional `unsafe` lines (count 29 → 32): the `alloc_contiguous(n)` path
for driver DMA arenas (§12) - the `unsafe fn alloc_contiguous` method, its inner
`&mut *addr_of_mut!(BITMAP)` access, and the public `alloc_contiguous` wrapper's
`(*addr_of_mut!(ALLOCATOR)).alloc_contiguous(n)` call. Sound for the same reason
as the rest of the allocator: every access holds `ALLOC_LOCKED` (single writer
across all cores), and the bitmap scan is bounds-checked against
`max_valid_frame`. `// SAFETY:` comments present in source for all three.

Five further `unsafe` lines (count 32 → 37): one from the page-table reclaim guard
(`phys_in_ram`'s `ALLOCATOR.max_valid_frame` read, commit `b9dbc4c`) and four from the
DMA permanent-reserve net (§12, the DMA-safety net): `unsafe fn alloc_dma_arena`, its
inner `self.alloc_contiguous(n)` call, the table-full `bitmap_set_free` undo, and the
public `alloc_dma_arena` wrapper's `(*addr_of_mut!(ALLOCATOR)).alloc_dma_arena(n)` call.
`alloc_dma_arena` records the run in `dma_reserves` so `free` skips it - the arena is
never returned to the general pool to be recycled as a page table (a stray DMA then hits
DMA-reserved memory, not a PTE). Sound for the same reason as the rest of the allocator:
every access holds `ALLOC_LOCKED`. `// SAFETY:` comments present in source for all five.

---

### memory/frame.rs

`Frame::from_phys` - constructs a `Frame` from a raw physical address. Sound
because all callers are in the frame allocator or page-table walker, both of
which obtain addresses from the validated Limine memory map.
*(SAFETY comment missing in source - needs back-fill.)*

---

### memory/mod.rs

Calls `set_hhdm_offset` with the Limine-supplied HHDM offset during early init.
Sound because this runs exactly once, on the BSP, before any AP or task sees
virtual memory. `// SAFETY:` comment present in source.

---

### memory/page.rs

`Page::from_virt` - constructs a `Page` from a raw virtual address. Used only
by the page-table walker with addresses derived from the HHDM. Sound for the
same reason as `Frame::from_phys`.

---

### smp/core.rs

Per-core ready-flag manipulation via static arrays indexed by `core_id`.
`core_id` is bounded by `MAX_CORES` at all call sites. `// SAFETY:` comments
present in source.

---

### smp/ipi.rs

APIC IPI delivery: reads `APIC_VIRT_BASE`, writes to APIC ICR register, and
dispatches IPI handler. Sound because `APIC_VIRT_BASE` is set during BSP init
before any IPI is issued; ICR writes follow the APIC specification (write high
word first, then low word to trigger). `// SAFETY:` comments present in source
for most blocks; a small number need back-fill.

---

### smp/mod.rs

AP startup via `start_all_aps`. Delegates to `arch/x86_64/ap_boot.rs`.
`// SAFETY:` comment present in source.

---

### smp/placement.rs

Round-robin core assignment reads the `READY_CORES` count set by `smp/core.rs`.
Sound because the count is written before placement is ever called (BSP marks
core 0 ready before spawning init). `// SAFETY:` comment present in source.

---

### smp/spinlock.rs

`SpinLock<T>` interior-mutable spinlock. Seven unsafe constructs:
- `without_interrupts(f)` - two blocks: `unsafe { pushfq; pop; cli }` to capture
  RFLAGS.IF and mask interrupts on the local core, and `unsafe { sti }` to restore
  the prior enabled state. Local-core, no memory effects, IF restored exactly (nests
  correctly). REQUIRED for locks taken in both syscall and interrupt context
  (`KSTACK_USED`): without it a timer firing mid-critical-section re-enters the lock
  in the ISR on that core and self-deadlocks (the `chaos max-carnage` freeze).
- `unsafe impl Send for SpinLock<T>`: sound because the atomic spinlock
  serialises all access to `T`; `T: Send` is required.
- `unsafe impl Sync for SpinLock<T>`: same reasoning - mutual exclusion is
  enforced by the atomic before any shared reference is handed out.
- `unsafe { &*self.lock.data.get() }` in `Deref`: sound because the lock is
  held (we have a `SpinLockGuard`); no other reference to the inner data can
  exist simultaneously.
- `unsafe { &mut *self.lock.data.get() }` in `DerefMut`: same reasoning for
  mutable access.
- `pub const ZEROED: Self = unsafe { core::mem::zeroed() }`: all-zeroes
  initializer for placing a large `SpinLock<T>` in `.bss` without the undef
  padding bytes that LLD rejects there. Sound only when the all-zeroes bit
  pattern is a valid `T` - the caller's responsibility via the `T` instantiated.
  Replaces a `core::mem::zeroed()` that previously sat in `ipc/routing.rs`
  (outside the permitted layers); moving it here keeps `ipc/` unsafe-free (§18.1).

`// SAFETY:` comments present in source for all five.

---

### interrupt/route.rs *(grandfathered)*

`pub unsafe fn deliver(irq: u8)` - called from the IDT stub with IF=0.
One unsafe line remaining (the `unsafe fn` declaration).
`IRQ_TABLE` is now protected by `SpinLock`; registration and delivery are safe
with respect to the lock. The `unsafe` on `deliver` reflects the interrupt-context
calling convention (must only be called from the IDT with IF=0).
`// SAFETY:` comment present in source.

---

### loader.rs *(grandfathered)*

ELF loader: two private helpers (`read_ehdr`, `read_phdr`) each contain one
`read_unaligned` call that copies the entire packed ELF struct into a local
value; all field accesses in `load()` then go through safe local copies with no
unsafe at the call site. The remaining two unsafe blocks are `write_bytes` (BSS
zeroing) and `copy_nonoverlapping` (segment copy); both are bounded by bounds
checks performed above them. `// SAFETY:` comments present in source for all
four blocks.

---

### main.rs *(grandfathered)*

Two unsafe blocks: (1) BSP stack switch via inline ASM - sound because
`BSP_BOOT_STACK` is a 512 KiB static buffer and the pointer arithmetic is
bounded; (2) deref of `boot_info_ptr` - sound because the Limine bootloader
guarantees alignment and validity. (The earlier COM2-init block was removed when
`com2_init` was made a safe function.) `// SAFETY:` comments present in source.

The H4 hardening calls (`install_kstack_guards`, `harden_hhdm_nx`) are **safe `fn`s**
(boot-ordering preconditions, not UB), so they add no `unsafe` here - see the
2026-06-08 reconcile.

---

### syscall/dispatch.rs *(grandfathered)*

2 unsafe lines remaining (reduced from 26):
- `pub unsafe extern "C" fn syscall_handler`: the raw ring-3 → ring-0 entry
  point installed as the LSTAR target; must remain `unsafe` because it
  processes untrusted register values from user space.
- `unsafe { map_in_active_tables(va, phys, flags) }` inside `handle_alloc_mem`:
  sound because `va` is in the task heap region (above `0x1_0000_0000`);
  `phys` is a freshly allocated frame from the bitmap allocator; the active
  page table is the calling task's own CR3. `// SAFETY:` comment present in source.

All other handlers were converted from `unsafe fn` to `fn` - their user-pointer
accesses moved to `arch/x86_64::read_user_bytes` / `write_user_bytes` which
encapsulate the unsafe in the permitted arch layer.

---

### task/mod.rs *(grandfathered)*

Seven unsafe blocks: two in the kstack pool - `kstack_pool_base` (`addr_of!` of the
`static mut KSTACK_STORAGE`, the single encapsulated pool-address read, reused by
`free_kstack`) and `alloc_kstack` (`(addr_of_mut!(...) as *mut u8).add(...)` slot-top
arithmetic) - and five in the spawn path (`write_bytes` for stack zeroing,
`task_cap_init_empty`, `write_bytes` + `*mut ServiceContextData` cast for the ctx
page, `TaskContext::new_user`, and `commit_task`). All bounded by prior bounds checks
or scheduler-layer invariants. `// SAFETY:` comments present in source.

The H4 kstack-guard install (`install_kstack_guards`) is a **safe `fn`**: it reads the
pool base via `kstack_pool_base()` and delegates the per-slot page unmap to
`page_tables::unmap_4k_strided` (the arch layer, §18.1), so it adds no `unsafe` here -
see the 2026-06-08 reconcile. (Centralising the pool-address read in `kstack_pool_base`
also let `free_kstack` drop its own `addr_of!` block, holding the net count at 7.)

The previous magic-word liveness scheme (`KSTACK_MAGIC_USED` volatile
reads/writes at slot offset 0) was replaced by `SpinLock<[bool; TASK_KSTACK_MAX]>`,
removing 5 unsafe lines.

---

### task/scheduler.rs *(grandfathered)*

36 unsafe lines. Five formerly-`static mut` arrays converted to atomic types,
removing eight standalone `unsafe {}` blocks (previous count was 42, then 38,
now back to the original 36 floor after `TASK_VALID` was also converted):

- `CORE_CURRENT` → `[AtomicUsize; MAX_CORES]`: removed standalone `unsafe` in
  `current_task_slot()`; accesses updated to `.load()`/`.store()`.
- `CORE_RR_SLOT` → `[AtomicUsize; MAX_CORES]`: removed both standalone `unsafe`
  blocks in `pick_next()`.
- `CORE_PENDING_KSTACK_LEN` → `[AtomicUsize; MAX_CORES]`: removed both
  standalone `unsafe` blocks in `drain_pending_kstack()`.
- `TASK_KERNEL_STACK_TOP` → `[AtomicU64; MAX_TASKS]`: removed the standalone
  `unsafe` in `prepare_ring3_switch()`.
- `TASK_VALID` → `[AtomicBool; MAX_TASKS]`: removed the standalone
  `unsafe { TASK_VALID[slot] = false; }` in `release_task_slot()` and the
  inline `if !unsafe { TASK_VALID[slot] }` in `for_each_active_cap()`. All
  stores use `Release` ordering; the lock-free `for_each_active_cap` read uses
  `Acquire` to pair with `Release` stores and ensure cap table visibility; all
  reads inside lock-protected regions use `Relaxed`.
- `CORE_PENDING_PML4` is `AtomicU64` so its load/store sites are safe - only
  the `Frame::from_phys` + `free_frame` pair and the `send_ipi` call needed
  `unsafe` blocks.

One remaining line in `for_each_active_cap` is still `unsafe`:
- `unsafe { TASK_CAP[slot].assume_init_ref() }.for_each_slot(&mut f)` - reads
  a `MaybeUninit<CapTable>` after `TASK_VALID[slot].load(Acquire)` returned
  `true`. Sound because the `Acquire` load pairs with the `Release` store in
  `reserve_task_slot`/`enqueue`, establishing that the `CapTable` write
  happened-before this read. `CapTable` cannot be const-constructed so
  `MaybeUninit` is necessary; `assume_init_ref` is the unavoidable unsafe
  assertion that the slot is initialised.

One additional `unsafe {}` block (count +1, net): `TASK_CORE` reads in
`pick_next` - the wake-hint fast path (`TASK_CORE[hint]`) and the RR scan loop
(`TASK_CORE[idx]`) both read this `static mut [u32; MAX_TASKS]` array. Sound
because `TASK_CORE[slot]` is written exactly once at spawn and never modified
thereafter (§9.1 static-placement invariant); all indices are bounded by
`MAX_TASKS`; reads are unsynchronised but safe because the value is immutable
after task spawn. Two new `unsafe` lines were added; one previously-unsafe
access to a now-atomic variable was removed, yielding net +1.
`// SAFETY:` comments present in source for both new blocks.

Sound in aggregate: all arrays are indexed by slot or core_id with bounds
checked at their call sites; ring3 switch is called only from the scheduler
with interrupts disabled; cap init runs before the task is visible to other
cores; deferred PML4 free runs only after CR3 switch.
`// SAFETY:` comments present in source for all blocks.

## 2026-07-15 - multi-arch stubs: aarch64 / arm / loongarch64 / riscv32 / riscv64 / s390x

Six per-arch scaffolds under `arch/<isa>/mod.rs`, added while proving the demarcation
(docs/multi-arch.md). Each is the arch layer (a permitted §18.1 layer, exactly like
`arch/x86_64/`) for a non-x86 target: the `_start` naked entry, a minimal boot bring-up,
and a UART poke. All `unsafe` in them is the same class as x86's arch layer - inline
`asm!` for the boot sequence and raw MMIO writes to a fixed UART register - and each block
carries a `// SAFETY:` comment. They exist only to compile (all six) and boot (four:
aarch64/riscv64/loongarch64 to a UART print, x86 to the full shell) the arch-neutral kernel;
no neutral file gained any `unsafe`. `arch/arm/mod.rs` and `arch/riscv32/mod.rs` are the
32-bit word-size proof (docs/multi-arch.md, "Word size"); `arm` needs no atomics shim
(ARMv7 LDREXD), `riscv32` uses `portable-atomic` (RV32A has no 64-bit atomic). Counts are
the current stub sizes; they may grow as a real port fills the arch surface, each increase
carrying its own `// SAFETY:` and an audit bump.


## `arch/arm/irq.rs` 13 -> 16 (+3): routing the USB interrupt to userspace (arm32 Phase 1)

`arch/` is a permitted layer (§18.1), so this is an inventory update rather than an amendment.

The three lines are the mechanism that lets a device interrupt reach a userspace driver on arm32:

| Line | Purpose | SAFETY argument |
|---|---|---|
| `mask_usb_irq` | write bit 9 to `IC_DISABLE_IRQS_1` | volatile write of ONE bit to the Device-mapped legacy IC. Writing 1 disables that line; 0s are ignored (the register is not read-modify-write), so it cannot disturb another line. |
| `unmask_usb_irq` | write bit 9 to `IC_ENABLE_IRQS_1` | as above, against the enable register. Reached from the `IrqUnmask` syscall. |
| `route::deliver(USB_VECTOR)` | hand the IRQ to the registered endpoint | called from inside the IRQ handler with interrupts masked, which is `deliver`'s documented contract. |

The mask pair is **load-bearing, not defensive**. The DWC2 line is level-triggered: it stays asserted
until the driver clears an HPRT change bit or a channel HCINT. The in-kernel driver never needed to
mask it because it cleared the condition inline, before returning. A userspace driver has not run
yet when the handler returns, so an unmasked line re-asserts immediately and the core makes no
further progress - which the liveness watchdog correctly turns into a panic.

This is also why `arch/arm/mod.rs`'s `ioapic::mask_vector` / `unmask_vector` stopped being no-op
stubs. They were harmless while every device interrupt was serviced inside the kernel; routing one
outward makes them the thing that holds the line off.
