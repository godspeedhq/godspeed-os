<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `hardware` - what this machine is, and what drives each part of it

Implementation shape: **shell built-in**, asking each device's owner - the kernel's introspection for
the cores and memory, `hw-enumerator` for the PCI bus, and the supervisor (`supcmd::DEVICES`) for which
service it runs for which device. Read only, always. The design, including everything not built yet,
is `docs/hardware-design.md`.

## Status, as built and honest (2026-10-08)

**Step 1 of the design's build order is built: the facts that already exist, no kernel change.** Run in
QEMU (`osdev test shell`), and on hardware on 2026-10-08: the Dell Wyse 5070 and the HP T630, where
the T630's view at `86f4e60c` reads right against its boot log - 22 PCI devices, every driver on the
device it drives, the dongle in `usb`.

Three runs to get there, each finding what QEMU could not. The Wyse showed its xHCI controller
driverless (the supervisor's answer left out the USB hosts, `c2bae9a1`). The T630 then lost every row
after the 24th: the commit that fixed it says the T630 has 19 PCI devices, which was the number the
truncated view SHOWED - it has 22, and the three hidden were two host bridges and the RTL8168, so the
NIC was missing too. It also showed `ehci`'s controller driverless and `audio-driver` on both HD audio
controllers (`86f4e60c`).

Built: the overview, the sections `cpu`, `memory`, `pci`, `soc`, `display` and `usb`, a comma list of
them, one device in full, the vendor and class names, and records when piped.

**Step 2 is built too, and with NO kernel change - by the operator's rule for this utility (2026-10-08):
`hardware` is read only and takes what the system already answers.** It adds, to one device in full,
the live command register, every BAR and the interrupt route, the capabilities its driver holds, and
what the driver's spawn asked for; and the views `<device> debug`, `<section> debug`, `interrupts`,
`why <device>` and `report`. QEMU-verified (`osdev test shell`), and on the HP T630 at `2149fc8c`:
`interrupts` read `xhci` and `audio-driver` on MSI vectors 0x30 and 0x31 to APIC 18, and `ehci`,
AHCI and the RTL8168 on legacy lines; `00:10.0 debug` decoded the xHCI controller's configuration
space - its MSI capability enabled, its MSI-X present but off - and its raw dump began `22 10 14 79`;
`why` named `xhci`'s confinement and `ehci`'s passthrough with their reasons. `report` ran there too:
every driven device in full - `xhci` holding `console_push`, asking for class 0x0c0330 with
confinement and an interrupt; `block-driver` BAR 5 unconfined; `ehci` by kind - and the cores. It
said of `wifi-usb` that the kernel "logged" a grant at spawn, when `wifi-usb` is granted no hardware at
all; a driver with no device word now says so instead.

Where those come from: the device's configuration space, read live by `hw-enumerator` (its op 4 - the
bus is where the kernel wrote each device's interrupt route, so reading it there needs no kernel
query); the driver's capabilities (`TaskCaps`, as `caps` reads them); each core's scheduler counts
(InspectKernel 6 and 7, as `observe` reads them); and, for `why`, the supervisor's spawn decision and
the reason it keeps beside its spawn rows (`supcmd::WHY`).

**What it does NOT show, because the kernel does not report it, and each view says so where it would
be:** the addresses of the grant as the kernel recorded them (a device's BARs are shown as the device
reports them; the kernel's own record of the window, the DMA arena and whether the IOMMU confined it
is in its boot log - `events log boot | match <driver>`, which the view names); how often each
interrupt fires; per-device IOMMU fault counts; each core's timer mode. A BAR's SIZE is not shown
either: reading it means writing all ones to the BAR, and this utility never writes.

**Not built**, each answering `designed, not built yet` rather than being mistaken for a device:
`problems`, `tree`, `events`, `firmware`, `compare` and `power`.

Two limits of step 1, stated: the `usb` section lists only the devices the supervisor starts a driver
for (today the USB WiFi dongle) - a keyboard or a stick a host drives itself, or a device nothing
drives, is not reported by its host yet; and a USB device is named by its IDs (`0bda:8176`), not by
its host and port.

## 1. Verbs

| Verb | Kind | What it does |
|---|---|---|
| `hardware` | report, pipes | a line for the machine, then every section this machine has |
| `hardware <section>[,<section>...]` | report, pipes | those sections; one this machine lacks says so |
| `hardware <device>` | report, pipes as lines | one device in full, named as `hardware` names it: its live registers, interrupt and authority |
| `hardware <device> debug` | report, pipes as lines | the device as the hardware sees it, read now: configuration space decoded, its capability list, and the raw 256 bytes |
| `hardware <section> debug` | report, pipes as lines | debug for every device in the section; `cpu debug` is each core's scheduler counts |
| `hardware interrupts` | report, pipes | each PCI device's interrupt route - MSI vector and target APIC, MSI-X, or legacy line - and its driver; records `device`, `route`, `driver` |
| `hardware why <device>` | report, pipes as lines | who drives it, why that service, and the reason the supervisor records beside its spawn row |
| `hardware report` | report, pipes as lines | everything, for a bug report: the overview, the interrupts, every driven device in full, the cores |
| `hardware help` / `hardware version` | | the house conventions |

## 2. Sections, and a section that is not there

`cpu`, `memory`, `pci`, `soc` (devices granted by kind on the Pis and the VisionFive), `display` and
`usb`, printed in that order.

- **The bare view leaves out a section the machine does not have.** A Pi 2 has no PCI bus, and a
  heading with nothing under it on every run is noise.
- **A section asked for by name is always answered**: `pci: none on this machine (no PCI bus the OS
  can read)`. Asking is not an error. In a pipe it is no rows, and the sentence goes to the console.
- **A section that is there but empty shows its heading and says so**: `usb` with nothing attached
  prints `(no device with a driver here attached)`. "The bus is not there" and "the bus is there with
  nothing on it" are different facts.

```
gsh> hardware
x86_64 - 4 core(s), 7 GiB

cpu
  DEVICE      KIND                 DRIVER         STATE       DETAIL
  core 0      cpu core             kernel         up          boot core
  ...
pci
  DEVICE      KIND                 DRIVER         STATE       DETAIL
  00:10.0     USB 3 (xHCI)         xhci           running     AMD 1022:7914, IRQ line 11
  00:18.0     host bridge          -              no driver   AMD 1022:...., no IRQ line
  ...

9 device(s) with a driver, 2 without - hardware <device> for one in full
```

**Which device a driver drives.** A driver the supervisor names by PCI class is given the FIRST device
of that class on the bus (the T630 has two HD audio controllers: the first shows `audio-driver`, the
second `not driven`). A USB host asked for by kind (`ehci`, `xhci`) is shown on its PCI controller
where the kernel resolves the kind on the bus, and in `soc` where it does not. A view that cannot hold
every row says how many it left out.

**One device in full**, as the QEMU machine's network card reads:

```
gsh> hardware 00:03.0
device     00:03.0
section    pci
kind       ethernet
id         8086:100e (Intel)
class      0x020000
driver     nic-driver
state      running, on core 1, restarted 0 time(s)
command    0x0107 (memory on, I/O on, bus master on, INTx on)
bar0       0xfebc0000 (32-bit memory)
bar1       0xc000 (I/O ports)
interrupt  INTx pin A, IRQ line 11
authority  nic-driver holds log_write, and 3 endpoint(s)
asked      class 0x020000, the first memory BAR, no confinement
granted    the kernel does not report its grant's addresses; it logged them at spawn - events log boot | match nic-driver
why        hardware why 00:03.0
```

## 3. Pipes (rule 12)

Every section is one record shape - `section`, `device`, `kind`, `driver`, `state`, `detail` - so any
combination pipes as one table: `hardware cpu,memory | count`, `hardware | where driver=-`,
`hardware pci | select device kind`. `interrupts` is its own table - `device`, `route`, `driver`. One
device, `debug`, `why` and `report` are labelled lines and pipe as text, to `match` or `write`; the pipe
holds 16 KiB and says when it cut a long `report`.

## 4. Failure says whose answer is missing

The supervisor not answering leaves the DRIVER column at `-` and the overview says so in a line of its
own; `hw-enumerator` not answering means no `pci` section. An unknown name answers `no section or
device '<name>'` and fails. A word of the design not built yet answers that it is not built yet and
fails, so `if hardware problems` cannot be read as an answer.

## 5. Tab completion (rule 9)

The built sections and the words `interrupts`, `report` and `why` complete; a device name is the
machine's, typed. No word completes to a path.
