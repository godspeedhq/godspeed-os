# 34. No non-x86 port can reach a disk in QEMU, so storage is testable on one architecture

**Status: OPEN, and it is a TEST-INFRASTRUCTURE limit rather than a defect in the OS. Each port fails
for its own concrete reason, measured today rather than assumed. The consequence is that every
storage guarantee this project makes is verified on x86-64 alone until a board is in front of
somebody.**

Found while trying to close the cross-ISA gate of `docs/gsfs-carnage.md` (3.11): write a GSFS volume
on one architecture, read and modify it on another, bring it back and compare. The x86 half works.
There is no second architecture to hand it to.

## What each port does today

| port | disk in QEMU | what actually happens |
|---|---|---|
| **x86-64** | **yes, AHCI** | Works. A volume was flashed, written and read back; this is what every `fs` suite uses |
| **riscv64** | **none attached** | `scripts/riscv_run.py` has no drive option at all. Its own comment notes the consequence: "with no disk, `fs` waits 30 s for block-driver" |
| **aarch64** (Pi 4) | attached, not visible | `scripts/pi4_run.py --drive` attaches a `usb-storage` device, and the script says plainly why it does not help: there is no VL805 (the Pi 4's PCIe-to-xHCI bridge) emulation, so the controller gets "no controller MMIO granted - idling" and the OS never sees the disk |
| **arm32** (Pi 2) | attached, never settles | `scripts/arm_run.py --usbdisk` attaches a stick. It enumerates, binds as mass storage and reports the right size - **and then re-enumerates, endlessly** |

## The arm32 case, measured

The stick is seen. It is seen 126 times:

```
dwc2-svc: port 1 - device CONNECTED
dwc2-svc: port 1 DEVICE via split - VID:PID=46f4:0001 class=0x00 speed=full addr=116
dwc2-svc: MASS STORAGE bound - bulk IN 1 OUT 2 mps 64 (ep0 mps 8)
usb: storage connected (port 1) - 16 MiB
   ... and again, addr=117, 118, 119 ... 127 ...
```

126 `device CONNECTED` events in about two minutes, the USB address climbing each time. The stick
binds, reports 16 MiB, and is gone again before anything can use it.

`block-driver` asks for capacity at ITS startup, which is before the first enumeration completes, and
records the answer:

```
block-driver: capacity 0 - the USB host service replied 1 byte(s), not a capacity: it is reachable
              with no disk bound
block-driver: no USB storage stick - NO disk (the SD card is the boot medium and is never written)
```

So `fs` comes up storage-unavailable and never mounts, even though `drives-info` later reports the
correct 32768 sectors - the capacity arrives, the mount does not follow it through.

**The control rules out the port and today's changes.** Booting the same kernel with no `--usbdisk`
gives a clean boot, `supervisor: ready`, a working prompt, and **zero** `device CONNECTED` events. The
loop appears only when QEMU's emulated stick is attached. Real Pi 2 hardware runs the storage stack
fine - that is how `selfcheck` reaches 349/0 on the board - so this is QEMU's dwc2 emulation, not the
driver on silicon.

## Why it matters beyond one gate

**Every storage guarantee is currently verified on one architecture.** The fifteen `fs` suites, the 75
torn-write tear points, the journal recovery measurements - all x86-64, all AHCI. The format is
little-endian by construction and the code is architecture-neutral, so the expectation is that it
travels. An expectation is not a result, and this is exactly the class of thing a user finds first.

It also means a whole category of bug is invisible to us: anything where the BLOCK TRANSPORT changes
the picture. AHCI hands `fs` a sector; USB mass storage hands it a sector through BOT/SCSI over a
split transaction. The filesystem should not care, and "should not" is the word this programme exists
to remove.

## What would close it, cheapest first

1. **Give `riscv_run.py` a disk.** The RISC-V `virt` machine can carry a drive, and the port already
   has a block path. This looks like the shortest route to a second architecture, and unlike the two
   ARM cases it is not blocked on emulating a specific piece of silicon.
2. **Find why the arm32 stick re-enumerates.** It may be a QEMU dwc2 quirk, or the driver's response
   to one. The address climbing on every cycle says the device is being re-addressed rather than
   recovered, which is a specific enough signature to chase.
3. **Make `block-driver` follow a capacity that arrives late.** Independent of the above and worth
   doing anyway: it asks once at startup and latches the answer. `fs` already has a request-driven
   re-mount for exactly this shape of problem (the LS1 self-heal); the driver does not have the
   equivalent. On real hardware a stick that enumerates slowly would hit the same path.
4. **Run the cross-ISA test on hardware.** The x86 half is done and the volume is written. Put that
   image on a stick, boot a Pi 2, read and modify it, bring it back. This is the one that produces a
   real answer rather than a QEMU one, and it is one of the reasons the hardware pass exists.

## The honest statement for now

`docs/gsfs-carnage.md` 3.11 stays **NOT RUN**, and it is recorded as **not reachable in QEMU** rather
than merely not done - which is a different fact and changes what a hardware pass is for. It is no
longer just a confirmation at the end; for this gate it is the only way to get an answer at all.
