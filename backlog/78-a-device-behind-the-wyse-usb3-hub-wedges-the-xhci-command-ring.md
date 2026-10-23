# 78. A device behind the Wyse's USB3 hub wedges `xhci`'s command ring, and the keyboard is lost until a root port changes

**Status: OPEN - FIX BUILT 2026-10-07 (`docs/wifi-usb.md` 44: USB3 hubs recognised by protocol, Set Hub Depth sent, their devices addressed at SuperSpeed), awaiting the Wyse card. Found 2026-10-07 on the Dell Wyse 5070 (`docs/wifi-usb.md` 41).**

## What the operator saw

With the WiFi dongle unplugged, unplugging the keyboard printed nothing and plugging it back in did not
bring it back. Plugging the dongle in brought both back at once.

## Evidence (`build/serial_output.log`, Wyse, 2026-10-07 13:01-13:04)

The board has four internal hubs on root ports: 1 (0bda:5411, the keyboard on its port 3 or 4), 6
(0bda:5415), and the USB3 halves 10 (0bda:0411) and 15 (0bda:0415). A dongle unplug re-enumerates the
whole controller. Each time it did:

```
xhci: port 15 (slot 5) is a hub - walking it for downstream devices
xhci: USB2 hub on port 15 (slot 5, 2 downstream ports, mtt=false, ttt=0)
xhci: hub configure (Hub bit, 2 ports, mtt=false, ttt=0) completion=1
xhci: command type 11 got no completion within its bound - its completion, if it comes, will be discarded
xhci: command type 9 got no completion within its bound - ...
xhci: hub port 2 connected but downstream Address Device FAILED (route/TT)
...
xhci: command type 15 got no completion within its bound - ...
xhci: endpoint slot 1 dci 1 repair command FAILED (cc None, state was 1)
xhci: hub slot 1 port 3 status probe -> None
```

Address Device (type 11) for the device on port 15's hub port 2 never completes, and no late completion
is ever logged. The command ring runs in order, so every command after it fails too - Enable Slot (9),
and the Set TR Dequeue (15) that repairs hub slot 1's control endpoint. With that endpoint dead, hub
slot 1 cannot be asked what changed, and the keyboard behind it is invisible. A root port change resets
the controller and rebuilds everything, which is why plugging the dongle back in recovers both.

**Only with the dongle out:** `xhci` has six device slices (`MAX_SLICES`). With the dongle bound all six
are taken before the walk reaches port 15's downstream ports (`out of DMA slices for a downstream device -
stopping hub walk`), so the device is never tried. With it out, one is free.

The device on hub port 2 is most likely the boot stick, the one SuperSpeed device in the machine. Not
confirmed.

## What is ruled out

- **The stale input context** (`41e98639`): fixed, and on the same run every root port addressed,
  ports 10 and 15 included. This is a different command on a different path (`address_downstream`).
- **The dongle**: the fault needs the dongle ABSENT, and the dongle and the keyboard share `xhci`
  without trouble while it is plugged in.
- **The T630**: it has no USB3 hub and no SuperSpeed device, and hot-plugs its keyboard on the same image.

## What the code showed, before `docs/wifi-usb.md` 44 changed it - read, not yet proved on hardware

- **The hub is walked as USB2.** `xhci` asks for the USB2 hub descriptor (0x29) and asks for the USB3 one
  (0x2A) only if that returned no ports. The Pi 4's VL805 answers nothing to 0x29 and is found as USB3;
  this Realtek hub answers 0x29 with two ports, so it is walked as a USB2 hub although it sits on a
  SuperSpeed root port (speed 4).
- **No SET_HUB_DEPTH is sent** to any hub. My understanding is that a USB3 hub needs it before it can
  route by route string to anything below it; that is NOT yet checked against the USB 3 specification or
  Linux's hub driver, neither of which is in this tree.
- **A command that never completes blocks the ring.** `run_command` gives up on it and goes on, but the
  controller does not: every later command queues behind it. The xHCI specification's mechanism for a
  command that will not finish is Command Abort (CRCR.CA, xHCI 4.6.1.2), which this driver does not use.

## The next concrete step

1. DONE (`docs/wifi-usb.md` 44): Linux's hub driver read, and `xhci` now decides SuperSpeed by the
   device's protocol, sends Set Hub Depth, and addresses devices on a USB3 hub at SuperSpeed. Awaiting
   the Wyse card.
2. Read xHCI 4.6.1.2 and implement Command Abort for a command that has no completion within its bound,
   so one stuck command cannot take the hub repairs with it. This is the general fix and is worth having
   even if step 1 is the specific one.
3. One Wyse card each, with a written prediction. No QEMU device emulates a USB3 hub, so all of it is
   hardware-only.

**Workaround until then:** on the Wyse, leave the dongle plugged in, or after unplugging it plug it back
in (or reboot) before relying on keyboard hot-plug.

## 2026-10-20: why a device plugged in at the BACK is not seen, found in the code (Wyse, `d4c5a750`)

The operator: on the Wyse, unplugging and replugging the keyboard works at the FRONT and not at the BACK -
the back sees the unplug and never the replug. The T630, on ehci and xhci, does both.

The front ports are root ports (3 and 4): a plug there changes a root PORTSC and raises an event, so it is
seen whatever is bound. The back ports sit behind the USB2 hub on root port 1 (0bda:5411), and a plug behind a
hub changes no root register: `xhci` sees it only by asking the hub (`hub_port_status`). It asks two kinds of
hub: those with a HID bound behind them (the loop over `devs[..ndev]`), and, when no HID is bound
(`ndev == 0`), the DISK's hub alone - written for the Pi 4, where the keyboard and the disk share one. On the
Wyse they do not: the stick is behind the SuperSpeed hub on root port 15. So once the keyboard is unplugged
from the back, `ndev` is 0, the back hub is probed by nobody, and the replug is invisible - though the log says
"hub on port 1 has nothing behind it yet - KEEPING it so an arrival can be seen" at every re-enumeration. The
log agrees: after the last unplug (21:46:49) the next minute shows 155 hub probes and no event, all of them on
the disk's hub.

This is very likely the "keyboard is lost until a root port changes" this entry opened with: the change at a
root port is what re-enumerated and found it. **The fix:** keep every configured hub - its slot, its slice, its
port count and its EP0 cursor - and probe the unbound ports of each on the hub tick, whatever is bound; not only
hubs that a HID or the disk happens to be behind. QEMU can show it (`usb-hub` with a keyboard behind it and a
stick behind a second hub, then removing and re-adding the keyboard from the QEMU monitor).

**Built 2026-10-20 (`c095c1ed`), QEMU-verified, and the Wyse card PASSED 2026-10-21.** Enumeration records every hub it keeps (`KeptHub`), and
the poll loop watches each kept hub with no HID, disk or dongle behind it, from an EP0 ring made coherent at
loop entry as the HID hubs' rings are. QEMU, `q35`, a keyboard behind one
`usb-hub` and a stick behind another: without the change the replugged keyboard was never found; with it,
"device arrived on hub slot 1 port 1 (a hub nothing was bound behind) - re-enumerating" and the keyboard back.

The card (`build/serial_output.log`, Wyse, 2026-10-21 01:36-01:37, image `c095c1ed`): five unplugs and replugs of
the keyboard, every one found again. Three at the back - back port 4 to 3, 3 to 4, and 4 to 3 again - each said
"hub slot 1 port N reads connected (1/2)", then "(2/2)", then "device arrived on hub slot 1 port N (a hub nothing
was bound behind) - re-enumerating", and "keyboard connected" 2.2 to 2.4 s after the first connected read. Two at
the front (root ports 3 and 4), seen as root-port changes as before. No panic, no service death, and no hub that
could not be readied or gave no answer.

**Found on the way, and left as it is:** the confirmation state the older scans share (`hub_seen`, `hub_tried`)
is kept by PORT NUMBER alone, not by hub and port. The first build of the watch used it too and never
confirmed: on that layout the keyboard and the stick are both on port 1 of their hubs, and every unanswered
probe of the stick's port reset the keyboard's count ("reads connected (1/2)", for ever). The watch keeps its
own state per hub now. The HID-driven scan and the disk scan still share theirs, so a device on port N of one
hub and another on port N of a second can disturb each other's confirmation there too; not fixed.

**Two hubs still watched by nobody, found by Audit 19 (2026-10-21) and not fixed:** the watch leaves out the
disk's hub, and the disk scan runs only while no HID is bound (`ndev == 0`), so with a keyboard on one hub and
the stick alone on another - the Wyse's layout - a device plugged in beside the stick is not seen while the
keyboard is bound. And a hub with only the WiFi dongle behind it is left out as the dongle's, while no scan of
the dongle's own exists, so neither an arrival there nor the dongle's unplug is seen. The exclusions should
hold only while the scan they defer to is running; the care is the same as for the watch, one cursor per ring.

**And one more, older than the watch, found the same day:** the other QEMU layout - the keyboard and the stick
behind the SAME hub, the Pi 4's shape, which the disk scan is meant to cover - does not see the keyboard's replug
either, with and without the watch (`HEAD` and the change both: "keyboard found again: False"). The watch leaves
that hub to the disk scan, as it must (two cursors on one EP0 ring desync it), so the fault is in the disk scan's
arrival arm, not yet located. The Pi 4 card of 2026-08 that showed hot-plug both ways predates the current
scans; whether it still holds there is not known.
