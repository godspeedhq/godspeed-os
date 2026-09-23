# 34. No non-x86 port can reach a disk in QEMU, so storage is testable on one architecture

**Status: OPEN for the two ARM rows ONLY. riscv64 is CLOSED 2026-09-20: it reaches a disk, formats
GSFS, and writes and reads files back in QEMU. Two real defects were found getting there and both are
FIXED - a driver that hung when its host service was silent, and a capacity that was latched at mount
and never refreshed. A third was found by accident and is the most serious of them: `fs`'s own
protocol selftest can format the live disk, and what prevents it is one argument.**

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

## The capacity latch (FIXED), and the selftest it nearly destroyed the disk with

`fs` mounts and serves, but `drives flash data` was refused on a disk that is present and serving:

```
fs: drives-info - capacity 32768 sectors, mounted false
fs: flash requested (capacity 0 sectors, forced false)
fs: flash REFUSED - block-driver reports 0 capacity
```

Two of our own statements disagreeing four lines apart on one boot. The cause is item 3 of the list
above - a capacity that arrives late and is latched - except the latch is in `fs`, not in
`block-driver`: `capacity` is a PARAMETER of `serve_once`, computed once at mount and never asked
again. `OP_DRIVES_INFO` shadows it with a fresh `block_capacity(ctx)`; nothing else does. On riscv64
the stick finishes enumerating after `fs` starts, so the latch stays 0 forever.

### The obvious fix is a disk-wiper, and this is why

Re-deriving inside `OP_FLASH` and `OP_RESET` makes riscv64 work end to end. **It also takes `fs-all`
on x86 from 25 of 25 to 2 of 25** - and the reason is worth every line of this section.

`fs` runs a `protocol_selftest` at startup that walks **every opcode from 0 to 255** through
`serve_once`, checking that no malformed request crashes it. Three of those opcodes are destructive:
**21 `OP_FLASH`**, **149 `OP_FLASH | 0x80`** (the forced variant) and **23 `OP_RESET`**. The only
thing that makes walking them safe is the third argument at its call site:

```rust
serve_once(ctx, vol, 0, false, p, 0, CapHandle(0), &mut out[..], &mut len);
//                   ^ capacity = 0, and every destructive arm refuses on it
```

An arm that asks `block_capacity()` for itself **ignores that injected zero**. The selftest then
formats the machine's real disk during boot, twice, before a prompt exists to object. Every suite's
disk was being wiped before its first command - which is why a suite that never types `flash`, like
`fs-check`, reported `1 files, 1 dirs` where it had baked two files and drifted the free count.

**How it was found, recorded because the reasoning was wrong three times first.** The symptom said
"reformatted"; the arm that reformats is never reached by `fs-check`; the only sender of `OP_FLASH`
anywhere is the shell's `drives flash`, which needs a typed `y`. Stack was ruled out (`serve_once`
12,888 -> 12,904 bytes of a 256 KiB budget), harness setup was ruled out (byte-identical `mkfs`,
bakes and drift). What settled it was logging the op byte of every request `fs` received and seeing
single-byte payloads counting 0, 1, 2, 3 - a walk, not a command. **Every theory was about who could
have SENT a flash; nothing had sent one, because the selftest calls `serve_once` directly.**

### What shipped

The refresh happens at the CALLER, in the live serve loop, where the selftest is not involved - and
only while capacity is still 0, so `read` and `write` never pay a round trip for a question already
answered:

```rust
if capacity == 0 {
    if let Some(n) = block_capacity(&ctx) { if n > 0 { capacity = n; } }
}
```

And the constraint is now written on `serve_once` itself, because nothing marked that parameter as an
injection point and that omission is the whole of this section: **`capacity` IS AN INJECTION POINT,
NOT JUST A NUMBER - do not re-derive it inside an arm.**

Verified: `fs-check` 9/9 (2/9 under the bad version, 9/9 baseline); `osdev test fs-all` **24 of 25,
the one failure `fs-blockdeath` being a precondition flake that passes 11/11 on a re-run** with `fs
noticed 202.3671ms after the kill`; riscv64 formats and mounts:

```
drives flash data
This ERASES the drive. Continue? [y/N] y
fs: flash requested (capacity 32768 sectors, forced false)
drives: formatted as GSFS - mounted, ready to use now (no reboot)
wrote /hello.txt (18 bytes)
riscv64-wrote-this
```

`docs/gsfs-carnage.md` 3.11 - write a GSFS volume on one architecture and read it on another - was
recorded as NOT REACHABLE in QEMU. It is reachable now.

### Still worth doing, and NOT done here

The selftest walking destructive opcodes is defended by exactly one argument value. That is fragile
in a way this entry has now demonstrated rather than predicted: the next person to touch those arms
has no reason to know. A structural guard - a flag on the call, or a read-only entry point the
selftest drives - would make it impossible rather than merely documented. Recorded rather than built,
because it is a change to the shape of the serve path and this branch has had enough of those
(§26.7).

## The two ARM rows, unchanged

`pi4_run.py` and `arm_run.py` use `raspi4b` and `raspi2b`, which emulate the real boards and have no
PCIe, so the qemu-xhci route that worked here is not available to them. Their measured causes above
stand.
