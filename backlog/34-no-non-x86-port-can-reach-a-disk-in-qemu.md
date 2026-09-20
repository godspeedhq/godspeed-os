# 34. No non-x86 port can reach a disk in QEMU, so storage is testable on one architecture

**Status: OPEN, NARROWED HARD 2026-09-20 - riscv64 now reaches a disk, mounts GSFS, and reads and
writes files in QEMU. The row below that said "none attached" is closed; what remains open is
FORMATTING on that port, and the two ARM rows. A real Commandment-level defect was found on the way
and is FIXED (see the end). It is no longer true that every storage guarantee is verified on one
architecture: it is true that every one involving `drives flash` still is.**

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


---

## 2026-09-20: riscv64 reaches a disk, and the reason it never had was not the one recorded

The row above reads *"riscv64 - none attached: `scripts/riscv_run.py` has no drive option at all"*,
and item 1 of "what would close it" is *"give `riscv_run.py` a disk"*. Both are now done, and the
route was not the one that looked obvious.

### The wrong device, confidently attached

The first attempt added an **AHCI** controller, on the reasoning that `virt` has a real PCIe host
bridge (an e1000 already works there) and `block-driver` already speaks AHCI. The boot looked
promising:

```
riscv64: pci ecam at 0x30000000, 2 device(s)
spawn[mmio]: 'block-driver' BAR 0x40000000 -> VA 0x100000000
spawn[dma]:  'block-driver' arena phys 0x84409000 -> VA 0x70000000 (64 KiB)
```

The kernel enumerated the controller, picked the right BAR, and granted the MMIO window and DMA
arena. Then `fs` timed out seven times at 30 s each and reported `capacity 0 sectors`.

**`services/block-driver/build.rs` maps `"aarch64" | "riscv64" => Some("xhci")`, and `main.rs` gates
`#[cfg(not(storage_is_usb))] mod ahci`. `ahci.rs` is not compiled on this port at all.** The kernel
had dutifully granted a driver an ABAR it would never read. A whole diagnosis was built on top of
that grant - including a measured, arithmetically correct argument that `LINK_WAIT_CYCLES`
(400,000,000) is ~200 ms at the T630's 2 GHz and 40 SECONDS at this machine's `timebase 10000000 Hz`,
which is true, and is about code that does not exist here. The fix made from it changed nothing,
because there was nothing to change.

**What settled it was the control, not more measurement**: booting with NO storage device attached at
all reproduced the identical failure. A theory about an AHCI constant cannot explain a failure on a
machine with no AHCI device, and no amount of further instrumenting the AHCI path would have said so.

### The right device

`-device qemu-xhci` plus `-device usb-storage`, which is the topology `storage_is_usb` declares.
`virt` has no USB bus of its own, so the controller is added explicitly; without it `xhci` reports
`no controller MMIO granted - idling` and there is nowhere for a stick to appear.

```
xhci: USB disk ready - 32768 sectors of 512 B (16 MiB)
xhci: USB disk sector 0 read OK - first bytes 00 00 00 00, sig=0x0000 (no MBR - raw or GSFS)
block-driver: USB mass storage serving block I/O (32768 sectors = 16 MiB)
fs: drives-info - capacity 32768 sectors, mounted false
```

## The defect the control found, and it was the real prize (FIXED)

With no USB host at all, `block-driver` **hung**. Not slowly - permanently, before it could serve
anything, logging nothing at all, not even its own no-disk line. `fs` then ate a 30 s timeout per
request and never reached `serving file API`.

`xhciblk::rpc` used `ctx.request_with_reply`, whose own SDK comment states it plainly: *"No deadline
on this variant, so `None` is always a lost peer, never a timeout."* An unbounded `call` wakes on a
reply or on the replier's DEATH (§8.6) - and a peer that is alive and idling is neither. `xhci` came
up with no controller, received the capacity request, and never answered.

**This is the rule above the others broken: a dependency that is missing, dead or silent must RETURN
with a loud "unavailable", never hang.** It affected every `storage_is_usb` board - Pi 2, Pi 4 and
VisionFive 2 - and x86 already had it right, because no AHCI controller means `serve_no_disk` answers
capacity with a truthful zero. `main.rs` even carries a long comment about fixing this exact defect
once before, on that path. It was fixed there and not here because **nothing had ever booted a
`storage_is_usb` board with no USB host** - which is this entry's own subject.

**Worth recording separately: `sectors()` already wrapped that call in a 20 s deadline loop, and the
bound was INERT.** A deadline around a call that never returns is never evaluated. It read as bounded
in review for as long as it existed.

Fixed: every question to the USB host service is a bounded `request_with_reply_call_err` now, with
two budgets (2 s for a capacity, answered from state the service already holds; 10 s for a read or
write that crosses BOT/SCSI to media), both under the 30 s `fs` allows, so a stuck peer is reported by
the service that knows WHICH peer. Retry is on `Err` only - the send failed, so nothing is
outstanding; a deadline is never retried, because the request may still be in flight and a second one
would leave the first reply to arrive as an orphan.

Verified: riscv64 with no storage now reports and SERVES (`USB mass storage serving block I/O (0
sectors = 0 MiB)`, 0 timeouts, was 4); riscv64 with a stick unaffected; `osdev test fs-all` **25 of 25
in ~31 min** on x86.

## What is still open on this port: FLASH

`fs` mounts and serves, but `drives flash data` is refused on a disk that is present and serving:

```
fs: drives-info - capacity 32768 sectors, mounted false
fs: flash requested (capacity 0 sectors, forced false)
fs: flash REFUSED - block-driver reports 0 capacity
```

Two of our own statements contradicting each other, four lines apart, on one boot. The cause is
found and is item 3 of the list above - a capacity that arrives late and is latched - except the
latch is in `fs`, not in `block-driver`: `capacity` is a PARAMETER of `serve_once`, computed once at
mount and never refreshed. `OP_DRIVES_INFO` shadows it with a fresh `block_capacity(ctx)`;
`OP_FLASH` and `OP_RESET` use the stale one. On riscv64 the stick finishes enumerating after `fs`
starts, so the latch is 0 forever.

**The obvious fix is NOT SHIPPED, and this is the useful part of the record.** Re-deriving in those
two arms makes riscv64 work end to end - flash accepted, `wrote /hello.txt (18 bytes)`,
`riscv64-wrote-this` read back, `dir` listing it. **It also takes `fs-all` on x86 from 25 of 25 to 2
of 25.** Reproduced and controlled: baseline 9/9 on `fs-check` four times, the change 2/9 twice and
4/9 with only the `OP_FLASH` half applied.

Ruled out so the next attempt does not re-derive them:

- **not the stack** - `serve_once` grows 12,888 -> 12,904 bytes against a 256 KiB budget;
- **not a taken branch** - `fs-check` sends only `drives check` and `read`, and the only sender of
  `OP_FLASH` anywhere is the shell's `drives flash`, so the inserted call site never executes;
- **not the harness setup** - the failing run's `mkfs`, both `bake` lines and the drift are
  byte-identical to a passing one.

And yet the symptom is that the disk the OS mounts has been REFORMATTED: fsck reports `1 files, 1
dirs, 76 blocks used` and `the free count already agreed with the tree - nothing was repaired`, where
a passing run reports `3 files, 1 dirs, 78 blocks used` and REPAIRS the deliberate drift. The baked
`/alpha.txt` and `/beta.txt` are gone.

A storage change whose failure mode cannot be explained does not ship, so this is recorded rather
than half-fixed (§26.7). The next attempt starts here, not from scratch.

## The two ARM rows, unchanged

`pi4_run.py` and `arm_run.py` use `raspi4b` and `raspi2b`, which emulate the real boards and have no
PCIe, so the qemu-xhci route that worked here is not available to them. Their measured causes above
stand.
