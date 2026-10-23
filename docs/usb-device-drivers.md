# USB device drivers on demand - HARDWARE-VERIFIED ON THE PI 2 (`dwc2`); `xhci` HARDWARE-VERIFIED ON THE PI 4 (2026-10-06)

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

The **binding generation** names one bind on that host. `usbfn::OP_INFO` returns it as well (built
2026-10-12, `2e3c4052`), so the started driver - and a driver comparing after any re-enumeration - can tell
"the same dongle, still bound" from "the same kind of dongle, bound again" (a replug, or a host re-scan that
reset it). It is taken from the machine's counter (`usbfn::next_gen`), so a restarted host does not repeat
its previous instance's, where that counter is calibrated and running. Before it, on `xhci` a
re-enumeration of the same dongle read as unchanged (`docs/wifi-usb.md` 25).

Supervisor to host, for the reconcile: `ASK` (`usbdev::ASK`, one byte, no reply capability). The host
answers with its ordinary report rather than a reply, so a late answer can never be mistaken for one of
the supervisor's commands, and there is one message carrying the state instead of two.

**As built (Pi 2, `docs/wifi-usb.md` 26):** `sdk/rust/src/service_context.rs` `usbdev`. One message,
`REPORT` - present or not, the binding count, VID:PID - instead of separate attached and detached
messages: each report is the host's whole state for its device, so a duplicate is harmless. The host is in
it since 2026-10-23 (byte 11, `HOST_DWC2`, `HOST_XHCI`, `HOST_EHCI`; section 4c); the port is not. Three hosts
report (`dwc2`, `xhci`, `ehci`), each with one such device. The binding
generation was a count restarting with each host instance until 2026-10-12; it is now the gen above, and
`OP_INFO` carries it.

## 3. What it changes

- **Not the kernel.** The supervisor already spawns and kills services; the hosts already send it
  nothing, and will send it this.
- **New grants, pinned with their reasons:** `dwc2` and `xhci` each gain the supervisor as a peer, for
  the reports (`ehci` too, 2026-10-23).
- **`wifi-usb`** is no longer started at boot. It is spawned with its dongle already bound, and the
  supervisor stops it on the report that the dongle is gone (decided in the first card: the driver does
  not exit by itself). On every board that embeds it, since `xhci` reports too (`docs/wifi-usb.md` 27),
  and that is now the Pi 4 and the VisionFive as well, beside their onboard radio.
- **`wifi status` with no dongle** says "no wireless radio", which is then literally true.
- **`utilities/56_wifi.md` 11** (`wifi hardware`) reads the radios that exist at that moment, and
  `nic-driver`'s bridge follows radios appearing and leaving, which the saved choice's fallback
  already describes.

## 4. The three hosts

| Host | Today | For a driver it does not run itself |
|---|---|---|
| `dwc2` (Pi 2) | binds the dongle, serves `usbfn` in full, tells `wifi-usb`, and REPORTS it (`usbdev`, 2026-10-06); the gen in `OP_INFO` (2026-10-12) | nothing |
| `xhci` (PCs, Pi 4, VisionFive) | U2a: binds the dongle, serves control transfers; REPORTS it and watches its root or hub port (`docs/wifi-usb.md` 27). U2b and U2c, the bulk IN and OUTs, hardware-verified on the Pi 4: the dongle joins WPA2 through the VL805 (28-32); the gen in `OP_INFO` (2026-10-12) | nothing |
| `ehci` (the T630's second controller) | the AMD hub on its root port with low-speed keyboards and mice behind it, as before; since 2026-10-23 also ONE vendor-specific device, behind that hub at any speed or straight on a root port, bound for a driver package and REPORTED as host `ehci` (section 4c) | the dongle: bound, but no notices are sent, so `wifi-usb` is not started for it |

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

**Superseded in part, 2026-10-23 (section 4c):** `ehci` now binds a vendor-specific device, the dongle among them,
and serves its transfers, for an installed driver PACKAGE. What stays true of the paragraph above: `wifi-usb` is not
started for a dongle on `ehci` - it waits to be told of each held transfer (`NOTE_BULK_IN`), and `ehci` sends no
notices - so `wifi` still reports no radio there, and the front sockets are still the answer for WiFi.

## 4b. A device for an installed driver package (2026-10-23)

The design above names "an interface class later" for the match table. What was built instead is narrower and
needs no class: `xhci` and `dwc2` bind, beside the dongle, any device whose first interface is vendor-specific
(class 0xFF) - the classes a host has no driver of its own for - and report it with the same `usbdev` report.
The supervisor matches the report against `USB_MATCH` AND against the installed driver packages that name a
device by `hw_usb`, and a package wins: it is started with the reporting host as its peer, and the built-in
driver of that device (`wifi-usb`, for the dongle) is kept down while it is installed. The package speaks the
function protocol through `gs::driver::bus`. One bound device per host still holds. Shown in QEMU with the FTDI
serial adapter behind `xhci` (`osdev test driver-packages`); `dwc2`'s side is built and unseen, since it serves a
high-speed device only and QEMU's `raspi2b` offers none. `docs/developer-services-design.md` 13.1 has the rest.

## 4c. `ehci` binds one too (2026-10-23)

Asked for by the operator the same day. `ehci` binds ONE device whose first interface is vendor-specific and that is
not a boot keyboard or mouse (which it drives itself), wherever it finds one: behind the T630's rate-matching hub at
high speed (straight through the hub) or at full speed (split through its translator), or straight on a root port.
It addresses it at 100, clear of the hub and the keyboards, configures it, finds its first interface's bulk
endpoints, and answers the function protocol for it: `OP_INFO` (with where it is), `OP_CONTROL`, `OP_CONTROL_ONCE`,
`OP_BULK_OUT` and `OP_BULK_IN`. `OP_REPLUG` is refused, as `xhci` refuses it. Every transfer is the async schedule,
one queue head at a time as its control transfer always was, in a part of the arena of its own; while keyboards are
polled the schedule is pointed back at their ring after each one (`services/ehci/src/bound.rs`).

**What differs from `xhci`, recorded rather than hidden (26.7).** No notices: a bulk IN waits up to 20 ms for the
device and says what came, nothing included, so a driver asks again (`gs::driver::bus` callers already do; `wifi-usb`
does not, which is why it is not started here). A device behind the hub is watched by asking the hub at most once a
second, and a device on a root port by reading the port on every wake (at most 250 ms apart); a re-scan for keyboards
leaves the bound device's port alone. As before, `ehci` resets only the FIRST connected root port at boot, and a root
port with nothing on it at boot is not watched for a plug.

**The report names its host.** `usbdev`'s report gained a byte, the host (`HOST_DWC2`, `HOST_XHCI`, `HOST_EHCI`),
because a PC now has two hosts that report and "nothing attached" from one said nothing of the other. The supervisor
keeps what each host last reported (`UsbState::by_host`, `Installed::usb_attached`): a package is started with the
host that HAS its device as its peer, its device's leaving is taken from that host's report only, and a built-in
driver is started for a device only on a host it is wired to - the dongle on `ehci` is said to be "attached on ehci,
which wifi-usb is not wired to", never handed to `wifi-usb` with `xhci` as its peer.

**Shown in QEMU** with QEMU's mass-storage device straight on a `usb-ehci` root port: `ehci` binds it (in a test build,
`bind-any-class`, since a stick is not vendor-specific and QEMU's EHCI has no vendor-specific high-speed device), the
supervisor starts `kind-stick` with `ehci` as its peer, and the package reads the stick's SCSI INQUIRY - a bulk OUT and
two bulk INs (`osdev test driver-packages`). **Not seen:** anything behind the T630's hub, the full-speed split path
above all, and the hub-port watch; those are a hardware card on the T630's back sockets.

## 5. Order of work

1. **Finish U2a on the T630:** the dongle's bring-up through `xhci` (DONE 2026-10-06, on the T630 and
   then the Pi 4, `docs/wifi-usb.md` 25 and 27).
2. **On-demand drivers, on the Pi 2 first** (DONE 2026-10-06, hardware-verified on the Pi 2: plug, unplug, replug, a kill and a supervisor respawn, `docs/wifi-usb.md` 26), because `dwc2` + `wifi-usb` is the hardware-verified path:
   the reports, the match table, stop-on-detach, the generation. Then the same on `xhci`, with the
   dongle's port watched (DONE 2026-10-06, hardware-verified on the Pi 4 rather than the T630, the
   operator's choice of test board; `docs/wifi-usb.md` 27).
3. **U2b and U2c:** receive and transmit through `xhci` (DONE 2026-10-07: both hardware-verified on
   the Pi 4, the dongle joined, `docs/wifi-usb.md` 28-32), then `nic-driver`'s radio bridge on x86 (DONE
   2026-10-07 on the RTL8168, hardware-verified on the T630 and the Wyse, `docs/wifi-usb.md` 39-41; the
   e1000, QEMU's, has none).
4. **`wifi hardware`** (`utilities/56_wifi.md` 11) once a board can have two radios: the report BUILT
   (2026-10-06, checked in QEMU), and `wifi hardware use` (DONE, both hardware-verified on the Pi 4,
   `docs/wifi-usb.md` 37).

Each step is a card on hardware, one change per flash, with its prediction written before it runs.
