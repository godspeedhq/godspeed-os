# USB device drivers on demand - BUILT ON THE PI 2 (`dwc2`), not yet on `xhci` (2026-10-06)

Agreed with the operator on 2026-10-06, while the USB WiFi dongle was being brought to `xhci`
(`docs/wifi-usb.md` section 7, U2): *"I would like the connected device to be recognised and the
appropriate driver/service loaded"*. `ehci` support for the dongle was asked for too, then set aside the
same day once its size was clear: *"if ehci is not compatible with the wifi dongle, then lets put an
honest limitation and we'll work on the xhci"* (section 4).

Until now a USB device's driver has been either inside the host driver (keyboards, mice, disks, the
Pi 2's ethernet) or a service started at boot whether or not its device is there (`wifi-usb`, which
idles until its host tells it a dongle is bound). This note is the third way: **a device that appears
gets its driver started; a device that leaves takes its driver with it.**

## 1. Who does what

The split is the whole design, and it is the constitution's split (CLAUDE.md 26.10: mechanism in the
host, policy in a service).

1. **The USB host reports facts, never decisions.** When a host (`xhci`, `ehci`, `dwc2`) enumerates a
   device it does not drive itself, it tells the supervisor: *attached - VID:PID, interface class,
   which host, which port*. When the device leaves: *detached*. A host never names a driver. If it
   could, a compromised USB driver could start any image it liked; reporting facts gives it nothing it
   did not already have.
2. **The supervisor decides, from its own table.** A match table maps a report to a driver image the
   supervisor already holds - `0bda:8176 -> wifi-usb` today, an interface class later. The supervisor
   spawns that driver wired to the host that reported it. The table is the policy, and it lives in the
   service that owns policy about what runs.
3. **A detach stops the driver on purpose, and it stays stopped.** Two cases the supervisor must keep
   apart, because today it restarts every managed service that dies:
   - the driver **crashes while its device is present**: restarted, as every service is;
   - the device **leaves**: the driver is stopped and NOT restarted, until the device is attached
     again.
4. **The supervisor's own restart reconciles.** A respawned supervisor (CLAUDE.md 6.2) asks each USB
   host what is attached, adopts the drivers already running for those devices, starts the missing
   ones and stops any whose device is gone - the reconcile it already does for every other service.

What stays as it is: the keyboard, mouse and disk paths inside the hosts. This is for devices a host
does not drive itself.

## 2. The protocol

Host to supervisor, one message each, `try_send`, no reply expected - the host must never wait on the
supervisor (8.9):
- `ATTACHED [host, port, vid, pid, class, binding generation]`
- `DETACHED [host, port, binding generation]`

The **binding generation** counts every bind on that host. It is also returned by `usbfn::OP_INFO`, so
the started driver - and a driver comparing after any re-enumeration - can tell "the same dongle, still
bound" from "the same kind of dongle, bound again" (a replug, or a host re-scan that reset it). Today a
replug of the same dongle reads as unchanged on both `dwc2` and `xhci` (`docs/wifi-usb.md` 25).

Supervisor to host, for the reconcile: `ASK` (`usbdev::ASK`, one byte, no reply capability). The host
answers with its ordinary report rather than a reply, so a late answer can never be mistaken for one of
the supervisor's commands, and there is one message carrying the state instead of two.

**As built (Pi 2, `docs/wifi-usb.md` 26):** `sdk/rust/src/service_context.rs` `usbdev`. One message,
`REPORT` - present or not, the binding count, VID:PID - instead of separate attached and detached
messages: each report is the host's whole state for its device, so a duplicate is harmless. The host and
port are not in it yet; one host reports today, with one such device. The binding count restarts with
each host instance. `OP_INFO` does not carry it yet.

## 3. What it changes

- **Not the kernel.** The supervisor already spawns and kills services; the hosts already send it
  nothing, and will send it this.
- **New grants, pinned with their reasons:** each USB host gains the supervisor as a peer, for the
  reports.
- **`wifi-usb`** is no longer started at boot. It is spawned with its dongle already bound, and the
  supervisor stops it on the report that the dongle is gone (decided in the first card: the driver does
  not exit by itself). On the Pi 2 today; on `xhci`'s boards it is still started at boot until `xhci`
  reports.
- **`wifi status` with no dongle** says "no wireless radio", which is then literally true.
- **`utilities/56_wifi.md` 11** (`wifi hardware`) reads the radios that exist at that moment, and
  `nic-driver`'s bridge follows radios appearing and leaving, which the saved choice's fallback
  already describes.

## 4. The three hosts

| Host | Today | For a driver it does not run itself |
|---|---|---|
| `dwc2` (Pi 2) | binds the dongle, serves `usbfn` in full, tells `wifi-usb`, and REPORTS it (`usbdev`, 2026-10-06) | the generation in `OP_INFO` |
| `xhci` (PCs, Pi 4, VisionFive) | U2a: binds the dongle, serves control transfers | the reports, the generation, the dongle's port watched (an unplug seen at once), then bulk IN (U2b) and bulk OUT (U2c) |
| `ehci` (the T630's second controller) - **limitation, not planned** | one topology: the AMD hub on its root port, low-speed keyboards and mice behind it; **skips every high-speed device** on a hub port | everything `xhci` needed, and bulk transfers from scratch |

**`ehci` - A RECORDED LIMITATION, not planned work (operator, 2026-10-06).** A dongle in a socket that
routes to `ehci` (on the T630, its second controller) is NOT driven: `ehci` logs it as a high-speed
device on a hub port and skips it, and `wifi` reports no radio. Plug the dongle into an `xhci` socket -
on the T630, the front ports. What closing it would take, recorded so the size is known when it is
picked up: it has a control transfer that already handles both directions, but it
clears the whole arena on every call and runs only during enumeration. It has no bulk transfers at all,
and it serves no requests - it only drains its queue. The dongle is a HIGH-speed device, which `ehci`
skips by design today ("mass storage / hub - not a HID"). The good news is that a high-speed device
behind a high-speed hub needs no split transactions, which is EHCI's simple case. The work: bind it
by VID:PID instead of skipping it; a runtime control transfer that leaves the rest of the arena
alone; a bulk IN qTD kept armed, with its completion taken on the interrupt; a bulk OUT; and serving
`usbfn`, as `xhci` and `dwc2` do.

## 5. Order of work

1. **Finish U2a on the T630:** the dongle's bring-up through `xhci`, the card in progress.
2. **On-demand drivers, on the Pi 2 first** (BUILT 2026-10-06, `docs/wifi-usb.md` 26; its card is owed), because `dwc2` + `wifi-usb` is the hardware-verified path:
   the reports, the match table, stop-on-detach, the generation. Then the same on `xhci` (T630), with
   the dongle's port watched.
3. **U2b and U2c:** receive and transmit through `xhci`, then `nic-driver`'s radio bridge on x86 (the
   PCs' NIC backends have none yet).
4. **`wifi hardware`** (`utilities/56_wifi.md` 11) once a board can have two radios.

Each step is a card on hardware, one change per flash, with its prediction written before it runs.
