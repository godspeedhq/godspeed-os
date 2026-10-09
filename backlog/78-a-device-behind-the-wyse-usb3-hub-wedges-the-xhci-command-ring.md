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
