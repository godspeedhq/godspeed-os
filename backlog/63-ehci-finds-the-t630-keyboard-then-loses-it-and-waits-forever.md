# 63 - `ehci` finds the T630 keyboard, fails to configure it, then waits for an event that cannot come

**Status: CLOSED 2026-09-27, fixed the same day it was opened - awaiting the confirming boot.** The cause
turned out to be one line, and one of the two "defects" was not a defect at all. It still puts the
sentence that CLOSED `backlog/11` in doubt, and that part stands.
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

## Defect 1: the configuration read had NO RETRY (OBSERVED, FIXED)

**The cause is one line, and it is not a budget or a mechanism.** The configuration-descriptor read called
bare `control`, and on failure `continue`d to the next port - abandoning the device. It is the **only**
transfer in the sequence that did not use `control_retry`, and it is the one most likely to fail: 64 bytes
over an 8-byte low-speed control endpoint is EIGHT split transactions, where the device descriptor above
was two or three.

`control_retry`'s own doc comment states the rule that call site was breaking: *"this hub's split control
endpoint is intermittently flaky - one failed SETUP must not abandon the device."* Every transfer in
`setup_hid` obeys it. The longest one did not.

Fixed: five tries, same as the rest, and a line naming the device if all five fail rather than a silent
`continue`.

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

## Defect 2: NOT A DEFECT - a deliberate trade with no bound (CORRECTED, then BOUNDED)

**This was wrongly characterised when the entry was written, and the correction matters more than the
fix.** I called it "a driver parked on an edge that cannot occur", which reads as an oversight. It is
not. `wait_for_connection`'s doc comment says it snapshots the already-connected ports on purpose,
*"otherwise a connected-but-unusable device would make the hot-plug loop spin (re-scan -> fails -> wait
-> still connected -> re-scan ...)"*. The author saw this exact case and chose losing a device over
burning a core.

What was genuinely missing is the third option, and 26.6 names it: the choice was between UNBOUNDED
retrying and none. A connected device that fails to come up now gets three more whole-enumeration
attempts with a settle between, and then parks exactly as before. With defect 1 fixed this path should
rarely be reached; it exists for a transfer that is flaky rather than broken, which is what this hub's
transaction translator demonstrably is.

The original description follows, as written.

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

## What remains

**The confirming boot.** Put the keyboard back in a T630 back port and check for
`ehci: *** boot KEYBOARD on hub port 4 ***` instead of `no boot keyboard/mouse attached`. Predicted: it
configures, because the transfer that failed now gets five attempts instead of one, and the device
descriptor read over the same endpoint already succeeded.

If it still fails, the retry line will say so by name (`config descriptor failed after 5 tries`) and the
re-scan lines will show three more whole attempts - at which point it IS `bugs/3`'s per-packet
sequencing and this entry should be reopened with that evidence. The two fixes are deliberately
distinguishable in the log for exactly that reason.

**The `backlog/11` doubt stands** and is not closed by this. That entry's collateral claim - "the keyboard
behind the hub still works afterwards" - did not reproduce, and whether 2026-09-21 was a lucky pass or a
regression is still unmeasured. If the confirming boot works, the most likely reading is that the single
attempt sometimes succeeded and sometimes did not, which is what the code always said about this hub.
