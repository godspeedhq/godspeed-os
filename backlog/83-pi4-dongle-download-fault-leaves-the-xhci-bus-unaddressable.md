# 83. Pi 4: the dongle's firmware-download fault leaves the bus behind the hub unaddressable, disk included

**Status: OPEN, 2026-10-12.** Seen on the Pi 4 at `ce7feea2` (`feat/ask`). Not caused by that branch's
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
   the USB disk is). The keyboard, on port 4, comes back.

Usually the next walk recovers. On 2026-10-12, after `kill all-services`, it did not: the dongle failed its
download, and for the rest of the run - over three minutes and 14 re-enumerations - every walk failed
Address Device on hub ports 1 and 2. The run ended `xhci: ... disk no` and
`block-driver: 'xhci' did not answer within 2 s - reporting storage UNAVAILABLE`. The machine lost its
disk and the dongle; the onboard radio carried the network.

## Evidence

`build/serial_output.log`, Pi 4, 2026-10-12 17:17-17:26 (three boots of one card). Lines to search: `cc=4`,
`could not be repaired`, `Address Device FAILED (route/TT)`, `disk none`, `storage UNAVAILABLE`.

## Ruled out

- **The `feat/ask` changes.** `2e3c4052` adds a 4-byte gen to `OP_INFO` and numbers binds by the counter;
  the failing transfer is an `OP_CONTROL_ONCE` firmware block, which it does not touch. The fault is
  recorded on the same board on 2026-10-07 (section 33), before any of it.
- **The dongle being unplugged.** It was not touched during the runs.

## What changed in exposure, said so it is not mistaken for a cause

Since `2e3c4052`, an `xhci` restart or re-enumeration gives the dongle a new gen, and `wifi-usb` brings it
up from cold - which is correct (the chip was reset) and is what made it work again after `kill xhci`.
But every bring-up is another download, and so another chance at this fault.

## Next step

Find what the walk after the failed dongle leaves in the hub's state: Address Device with completion 4 for
the hub's OTHER ports says the hub (its TT, or the controller's slot context for it) is what is wrong after
the re-scan, not those devices. Instrument the hub's slot and port state before and after the dongle's
repair and the re-scan. Section 38's port-local handling (reverted: on the Pi 4 it lost the keyboard) is
the record of one attempt.
