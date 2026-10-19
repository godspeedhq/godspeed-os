# 91. A driver is replaced cold; a hot handover for drivers is designed, not built

**Status: OPEN, 2026-10-17.** A decision recorded, not a defect: the operator asked for the design to be kept.

## What is true today

A service whose old and new versions both declare `handover` is upgraded HOT: the new version starts beside
the running one, takes its state over IPC, and nothing stops (CLAUDE.md 2.5 amendment 2026-10-17,
`docs/developer-services-design.md` 14). A driver is always COLD: the shared check refuses a driver whose
contract declares `handover` (`services/godspeed-pm/src/check.rs`), so its upgrade stops the old version,
checks and starts the new one, and the new one resets the controller and enumerates again.

The other kinds need nothing here. A utility runs for one command, so the next run is simply the new version.
An app is replaced cold on purpose: a new version may have a new interface, so the honest move is the one
Chrome, VS Code and GitHub Desktop make - tell the user and let them reload.

## Why it matters

A service that depends on a driver goes through its own upgrade with no disruption, and then through its
driver's with one: a USB keyboard or disk disconnects and comes back, a radio rejoins (about 7 s on the
Pi 4), storage pauses. The dependent already survives this, because a cold upgrade is the same event as a
driver crash and every client must recover from that (14.3). So nothing breaks; the cost is a visible blip.

## What is RULED OUT

- **Side by side, as a service does it.** Two instances would both program one device. The device is shared
  state with exactly one owner, and that rule is not relaxed.
- **Loading new code into the running driver.** Rejected permanently by 2.5: the bytes that run are the bytes
  checked from the disk.

## The design, if it is ever needed: a relay, not an overlap

The raw pointers that make a hot driver reload hard in Linux (function tables and callbacks other kernel code
holds into a module) do not exist here: a driver is an isolated program, and clients reach it by name. The
pointers that matter are the DEVICE's - rings, descriptors and buffers written as physical or IO addresses in
the DMA arena - and those survive as long as the arena's pages do.

1. **Quiesce.** The old version stops taking requests, lets in-flight transfers complete or time out, stops
   the rings, masks the interrupt and leaves the controller in a known, described state.
2. **Describe.** It sends the successor a state message: enumerated ports and slots, ring positions, the
   arena's layout as OFFSETS, never addresses. Device-defined formats (the xHCI ring) are fixed by the
   hardware spec and need no conversion; only the driver's own bookkeeping does.
3. **Move the grant.** The kernel moves the MMIO window, the arena's frames, the IOMMU domain and the interrupt
   route from the old task to the new in one step, without freeing the arena. Moved, never copied, so one task
   owns the device at every moment.
4. **Resume.** The new version adopts the device as described and restarts the rings without a reset. Clients
   see a pause, not a disconnect.

## What it costs

A kernel change: moving a device grant between two live tasks. Grants are made only at spawn, from the device
class, and released at death; a move widens the surface Commandment I pins and needs its own amendment. The
kernel must keep the arena's frames alive across the move and re-point the IOMMU domain and the interrupt
route atomically. Every driver that opts in must write a real quiesce, which is the hard engineering: some
devices cannot cancel a DMA in flight, so the arena must stay mapped and valid until the device is done.

## A cheaper step first

`wifi-driver` already resumes from the DEVICE's state instead of resetting it: a respawn adopts the firmware
the dead instance left running on the chip. Doing the same on an upgrade - the new version adopts what the
hardware still holds, where the hardware holds enough - needs no kernel change and may remove most of the
blip for the drivers where it is worst.

## The next concrete step

None until something needs it (26.2). The trigger is a driver whose cold restart is user-visible AND whose
upgrades are frequent enough to matter; then try the cheaper step on that driver first, and build the relay
only if adopting the hardware's state is not enough.
