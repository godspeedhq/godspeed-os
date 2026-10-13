# 83. Pi 4: the dongle's firmware-download fault leaves the bus behind the hub unaddressable, disk included

**Status: OPEN, 2026-10-12; the hub cut is built and ran on the card 2026-10-13** - bounded and safe for
the disk, and it did not revive the dongle (*The card*, below). Seen on the Pi 4 at `ce7feea2` (`feat/ask`). Not caused by that branch's
changes: none touches `xhci`'s control-transfer, repair or walk paths. `docs/wifi-usb.md` 52 has the run.

## What happens

The RTL8188CUS's firmware download, through the Pi 4's `xhci` (VL805, the dongle behind its internal
hub), fails on a 128-byte control OUT with `cc=4` (USB Transaction Error). This is the intermittent
fault `docs/wifi-usb.md` 27 and 33 record; on 2026-10-12 it failed in 6 of 9 Pi 4 bring-ups, and 10 of 19
counting the ones in section 33. What follows it is the defect here:

1. `xhci` repairs the dongle's EP0 up to 8 times; every transfer after a repair times out with the
   dequeue at the ring's start (`EP0 repaired ... endpoint state 3, dequeue Some((0, 1))`).
2. It gives up and re-scans the whole controller (`its control endpoint could not be repaired -
   re-scanning (unplugged?)`), the section 37 behaviour.
3. In the walk, devices on the hub's other ports fail Address Device with completion 4
   (`hub port 1 connected but downstream Address Device FAILED (route/TT)`, the same for port 2, where
   the USB disk is). The keyboard, on port 4, still answers Address Device (see below for what it
   does after that).

Usually the next walk recovers. On 2026-10-12, after `kill all-services`, it did not: the dongle failed its
download, and for the rest of the run - over three minutes and 14 re-enumerations - every walk failed
Address Device on hub ports 1 and 2. The run ended `xhci: ... disk no` and
`block-driver: 'xhci' did not answer within 2 s - reporting storage UNAVAILABLE`. The machine lost its
disk and the dongle; the onboard radio carried the network.

## Evidence

The 17:17-17:26 run (three boots of one card) was in `build/serial_output.log` when this entry was
written; that file has since been overwritten by the 20:09 run below, which shows the same thing with the
instrument in. Lines to search: `cc=4`, `could not be repaired`, `Address Device FAILED (route/TT)`,
`DIAG`, `disk none`, `storage UNAVAILABLE`.

## Ruled out

- **The `feat/ask` changes.** `2e3c4052` adds a 4-byte gen to `OP_INFO` and numbers binds by the counter;
  the failing transfer is an `OP_CONTROL_ONCE` firmware block, which it does not touch. The fault is
  recorded on the same board on 2026-10-07 (section 33), before any of it.
- **The dongle being unplugged.** It was not touched during the runs.

## What changed in exposure, said so it is not mistaken for a cause

Since `2e3c4052`, an `xhci` restart or re-enumeration gives the dongle a new gen, and `wifi-usb` brings it
up from cold - which is correct (the chip was reset) and is what made it work again after `kill xhci`.
But every bring-up is another download, and so another chance at this fault.

## What the instrument showed (2026-10-12, 20:10-20:17)

`xhci` now logs a DIAG line for every hub port that fails Address Device (51 in this run, the first at
20:10:46, the last at 20:16:51). DIAG prints only on a failure, so it says nothing about a passing walk.
In every failing one the hub and the port read as normal:

- the port reads `0x0101` before its reset and `0x0503` after it (connected, enabled, powered, high speed),
  and still `0x0503` after the failure;
- the hub's slot is Addressed (state 2; Configured would be 3, and needs an endpoint beyond EP0), has 4
  ports and its hub flag set, and its EP0 is running.

So neither the hub's slot context nor its TT state is visibly wrong. The device on the port answers its
port reset and does not answer SET_ADDRESS. This was seen on port 1 (the dongle), and in some walks on
port 2 (the disk) as well.

The hub reports `port power ganged, power good after 100 ms` (wHubCharacteristics bits 1:0 = 00). The
per-port power cycle built first therefore never ran: a ganged hub has one power switch for all four
ports.

**The keyboard does not come through unharmed either.** It answers Address Device in every walk, but while
the dongle is wedged its configuration descriptor read often fails (`Get Config Descriptor failed`, 20:11:18,
20:11:39, 20:12:03 and later), and it bound only at 20:12:41, 20:13:18 and 20:16:51 after 20:10:54. Once,
at 20:11:02, the device on port 4 answered its device descriptor with the HUB's own identity (VID 0x2109,
PID 0x3431, class 0, speed 2), and was walked as a hub. That is worth reading as a clue, not a curiosity:
an answer from the wrong device on the wire.

**Not established:** why the disk on port 2 also fails while the dongle is wedged, and what made port 4 read
the hub's descriptor.

**An inference, not checked:** every re-scan already resets the hub at its root port, and under USB 2.0 a
hub reset leaves switched ports Powered-off until the walk powers them again. Thirty-odd re-scans did not
revive the dongle. So either this hub does not really switch VBUS, or the window between the reset and the
walk's power-on was too short for the device to drain. The 1 s cut below tests the second; if the first
is true, it will not help.

## What is built (on hardware since 2026-10-13, below)

A cold start of the hub, bounded:

1. **A device that three Address Devices do not reach is left alone.** Its slot is released, also when
   Address Device gets no completion at all (this used to leak: three slots per failed port, until the next
   controller reset), and its port is disabled (not on a SuperSpeed hub, which has no PORT_ENABLE feature
   and would stall the request).
2. **The next walk starts from cold.** It cuts the power of every port on a ganged hub, or of only the
   failed ports on a USB2 hub that switches each port on its own (a SuperSpeed hub is treated as ganged).
   It waits 1 s, or twice the hub's power-good time if that is longer, logs what the hub then reports (how
   many ports read unpowered, how many still connected), and powers the ports again. This drops the disk
   and the keyboard as well. They come back the way a restarted service does: `fs` recovers through its
   journal, and a write in flight fails loudly to its writer. On a drive that attests its cache flush, as
   this stick does (CLAUDE.md 6.1, 2026-09-23), a write `fs` has acknowledged is already on it; `fs`'s
   barriers are advisory, so on one that does not, that is not promised.
3. **Bounded once per wedge, per hub, with no cap on how many wedges.** A walk of a hub already cut does
   not ask for another, so a wedge that survives a cut ends the cutting. A walk of that hub in which every
   connected port was tried and answered ends the wedge; a walk that skipped a port (its status unread, its
   reset not enabling it, the ring or the slices run out) does not. The cut is recorded when it is made,
   and a request whose hub does not come back in the next pass is dropped. A cap of 3 cuts was built and then removed, the same day. A count binds only the case where
   the cuts work: a dongle revived by a cut, then wedged again by its next failed download (about 6 in
   10). Giving up there loses the disk, which fails to address while the dongle is wedged. Like the
   supervisor's respawn (CLAUDE.md 6.2), each cut costs what the last one did and is logged with a running
   count. If that count climbs on hardware, the bound belongs on the failing device (stop starting the
   dongle after its downloads keep failing), not on the cure.
4. **What a device that still fails is told depends on what the hub reported after the cut**, as far as
   that can say. Unplugging a device removes its power and nothing else, so it is the same as a cut that
   really drops the power. But the port status is the hub's LOGICAL state (USB 2.0 11.24.2.7.1.6: PORT_POWER
   "may or may not represent whether power is applied"):
   - if the hub read its ports still powered, it proved the cut was not a cold start, and unplugging the
     device or moving it to another port is the only one available;
   - if it read them off and nothing connected, it proved only that it took the request. The line says
     both readings: if the power really dropped, the device fails from cold and unplugging will not help;
     if the hub only said so, unplugging is the cold start this could not make;
   - if the status could not be read, it says that.

Linux power-cycles the one port half way through its `PORT_INIT_TRIES` (4) attempts
(`usb_hub_set_port_power`), with no check of how the hub switches power. On a ganged hub that one-port
clear drops nothing, because a gang is off only when every port in it is (USB 2.0 11.11). Cutting every
port is this driver's choice, made because what drops with it has a recovery (CLAUDE.md 26.14).

## The card (Pi 4, 2026-10-13, `build/serial_output.log`)

`repeat count 6 wait 30 kill xhci`, the dongle, the stick and the keyboard on the hub. Every restarted
`xhci` found the dongle wedged in the same way - a fresh bind, then `cc=4` in the firmware download and
an EP0 the repair could not revive - and then:

```
08:32:22.782  xhci: hub on port 1: port mask 0x2 does not answer after a reset - cutting the power of ALL its ports ... (cut 1 of this instance)
08:32:24.133  xhci: hub on port 1: power cut on 4 port(s) for 1000 ms - the hub reports 4 of 4 read port(s) unpowered and 0 still connected (its logical state); powering them again
08:32:30.009  xhci: hub on port 1: port mask 0x2 still does not answer after a cut the hub reported as done - ...
```

- **Bounded as designed.** One cut per instance: six restarts, six cuts (the boot's own instance found no
  wedge - the dongle joined then); each later walk of that hub said "still does not answer after a cut"
  and did not cut again.
- **The hub reported the cut done** every time: 4 of 4 unpowered, 0 connected.
- **The disk and the keyboard came back every time**: `USB disk ready - 31266816 sectors` and the
  keyboard bound, after each cut and each walk. The disk was never lost.
- **The dongle did not.** Address Device failed on its port after every cut.

**What this does not settle.** The disk re-reporting its capacity and the keyboard rebinding happen on
every walk anyway (each walk resets the hub), so they do not show the power dropped, and the hub's port
status is its logical state. A hub that records the cut and keeps VBUS on reads exactly like this.

## Next step

Unplug the wedged dongle and plug it back in, on a boot where it has wedged. That is the cold start a
cut that really drops the power would have been:

- it comes up: this hub does not switch VBUS, and a cut cannot help on this board. The cut then costs a
  second and a rescan for nothing, and the next remedy is the root port's own power (PORTSC.PP, where the
  controller has port power control);
- it stays dead: the dongle fails from cold, and unplugging will not help either.

If the hub does not drop its power, the next remedy is the root port's own power (PORTSC.PP, where the
controller has port power control). Section 38's port-local handling (reverted: on the Pi 4 it lost the
keyboard) is the record of an earlier attempt.
