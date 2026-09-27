# 63 - `ehci` finds the T630 keyboard, fails to configure it, then waits for an event that cannot come

**Status:** OPEN. Two defects, one observed on hardware and one read from the code and consistent with the
reported symptom. It also puts the sentence that CLOSED `backlog/11` in doubt.
**Found:** 2026-09-27 on the HP T630, by the operator: *"ehci with the keyboard didn't work. hotplug
didn't work. I had to connect it to xhci (in front)."* Serial evidence below, from a boot that was
otherwise clean.

## What the machine actually did

```
ehci: port 1 after reset: PORTSC=0x00001007 enabled=1 -> HIGH-SPEED (hub ...) -> E3b enumerates it
ehci: DEVICE DESCRIPTOR class=0x09 proto=1 (TT type) mps0=64 VID=0x0438 PID=0x7900
ehci: HUB DESCRIPTOR ports=4 characteristics=0x0069
ehci: hub port 1 enabled high-speed (mass storage / hub) - not a HID, skipping
ehci: hub port 2 enabled high-speed (mass storage / hub) - not a HID, skipping
ehci: hub port 3: status=0x0100 connected=0 low_speed=0
ehci: hub port 4: status=0x0301 connected=1 low_speed=1
ehci: SPLIT device (hub port 4): VID=0x046d PID=0xc30a
ehci: a control transfer ran out its full budget (device gone?) - parking between polls
ehci: no boot keyboard/mouse attached - waiting for a connection
```

**The keyboard was FOUND.** `046d:c30a` is a Logitech device, low-speed, behind the AMD hub
(`0438:7900`) on hub port 4, reached through the transaction translator by split transactions. Its
device descriptor was read - the VID and PID are on the console, so the split path worked at least once.

Then a later control transfer ran out its budget and the driver gave up. It never reached
`ehci: port N HID iface=...`, which is printed after the CONFIGURATION descriptor is parsed - so the
failure is between reading the device descriptor and reading the configuration descriptor. A low-speed
control endpoint has an 8-byte maximum packet, so a ~60-byte configuration descriptor is eight split
transactions where the device descriptor was two or three.

## Defect 1: the split configuration read fails (OBSERVED)

The code already knows this endpoint is unreliable - `control_retry` exists with a five-try budget and its
doc comment says *"this hub's split control endpoint is intermittently flaky"*. Five tries were not
enough here.

**And it is the same FAMILY as `bugs/3`**, which is worth more than the resemblance suggests, because that
one was root-caused and fixed. `bugs/3_DWC2_SPLIT_XACTERR_LOWSPEED_KBD.md` is a low-speed keyboard behind
a hub on the Pi 2, and its finding was that the controller *"cannot sequence a multi-packet transfer over
a SPLIT... it halts XferCompl after the first low/full-speed packet"*, fixed by sequencing one
maximum-packet-sized packet per split transaction. The mechanism differs - EHCI's hardware TT is supposed
to do that sequencing itself - but the failing shape is identical: short split transfers work, longer ones
do not.

**This contradicts a theory already written into the source.** A comment in `ehci` attributes T630 back-port
trouble to *"the T630 hub has dead low-speed ports"*. The port is not dead: it reported
`connected=1 low_speed=1` and answered a descriptor read with its real VID and PID. Enumeration got far
enough to name the device, so what fails is the transfer path, not the socket. That matters because a
"dead port" is somebody else's fault and needs no work, which is exactly why a wrong diagnosis of that
shape is expensive.

## Defect 2: after the failure it waits for an event that cannot arrive (READ FROM CODE)

```rust
loop {
    let (devs, ndev) = scan_devices(...);
    if ndev == 0 {
        ctx.log("ehci: no boot keyboard/mouse attached - waiting for a connection");
        wait_for_connection(...);
        continue;
    }
```

`wait_for_connection` waits for a connection CHANGE. The keyboard never disconnected - it was connected
before boot and stayed connected - so no change event is coming, and the driver sits waiting while a
device it failed to configure is plugged in front of it. That is the reported "hotplug didn't work": not a
missing hot-plug path (the loop is a hot-plug path) but a driver parked on an edge that will not occur.

A physical unplug-and-replug DOES produce the event, and then defect 1 recurs, which is consistent with
the operator finding that replugging changed nothing.

**Confidence:** defect 1 is observed on hardware. Defect 2 is read from the source and matches the
symptom; it has not been instrumented, and it should be before anyone changes the loop.

## And it puts `backlog/11`'s closing evidence in doubt

`backlog/11` (the EHCI BIOS handoff) was **closed on 2026-09-21** having been executed on this same T630,
and its closing text says: *"The keyboard behind the hub still works afterwards, which was the stated
fear."* Six days later, on the same board and the same hub, it does not.

The handoff itself is not in question - it worked, loudly and as designed (`USBLEGSUP@0xa0 OS-owned, BIOS
released=0 ... FORCED after timeout`). What did not reproduce is the collateral claim. Given the code's own
"intermittently flaky" note, the 2026-09-21 observation may simply have been a lucky pass, and one pass is
not evidence of a property - which is the same trap `backlog/62` is open about.

**Not reopening 11 blindly:** its subject is the handoff and that stands. But its closing sentence should
not be read as "the keyboard works on EHCI", and this entry is the reason.

## Next step

1. **Count it.** Boot the T630 with the keyboard on a back (EHCI) port five times and record how many
   configure. The code claims intermittency and two observations disagree; a rate is the missing fact.
2. **Widen the budget before changing the mechanism**, because it is the cheap experiment: raise
   `control_retry`'s tries and the per-transfer budget for the CONFIGURATION read specifically, and see
   whether it ever completes. If it does, the fix is a budget; if it never does, it is `bugs/3`'s
   per-packet sequencing and that is real work.
3. **Fix defect 2 regardless**, since it is a logic error independent of the transfer: after a failed
   configure of a device that is still connected, retry the device rather than waiting for a connection
   change that cannot arrive. Bound the retries and say so, per 26.6.
4. The workaround is real and should be written where an operator reads it: **on the T630 the keyboard
   belongs in a front (xHCI) port.** `xhci` bound it without trouble on this boot - `1 HID device(s)
   bound`.
