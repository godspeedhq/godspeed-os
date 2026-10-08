<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `hardware` - what this machine is, what drives each part of it, and what is wrong

**Status: STEP 1 BUILT (2026-10-08), the rest designed.** Agreed with the operator in conversation on
2026-10-08, during the T630 cards of `docs/wifi-usb.md` 51. Step 1 of the build order (section 14) is
built and its spec is `utilities/58_hardware.md`, which is what the shell answers; this note stays the
design for everything after it.

**Two decisions made while building step 1:**
- **A section the machine does not have is left out of the bare view; asked for by name, it is
  answered** ("pci: none on this machine"); a section that is there but empty shows its heading and
  says so. "The bus is not there" and "the bus is there with nothing on it" are different facts.
- **The raw boot log does not belong here.** It is a log, and logs are read with `events log`; two
  commands for one log is two ways to ask. What neither the sink's window nor the kernel's 16 KiB ring
  could do was keep the boot - both move on, so after a chaos run the boot lines were gone. That is
  `events log boot` - a fixed copy of the boot output the kernel keeps - agreed with the operator the
  same day and built as its own change (`utilities/47_events.md`). `hardware` keeps what the boot FOUND (the bus, the IOMMU, the timer mode), as facts. Every example below is a
MOCKUP: values seen in the T630's logs that day are real, and times, counts and anything not yet read are
illustrative.

## Why

The facts already exist and are scattered: `cores`, `mem`, `drives`, `wifi hardware`, `audio`, `net` and
the boot log each hold a piece, and nothing answers the whole question - what is this machine, and what
is driving each part of it. Three bugs found on one day (2026-10-08) were each a question this utility
would have answered in one line, and each was instead dug out of a 20 MB serial log:

- **which device each driver was given** - a restarted supervisor handed every driver the previous
  driver's device (`c300666c`);
- **which devices the IOMMU confines, and in which domain** - every confined device shared domain 1
  (`c51245c6`);
- **where each device's interrupts land, and when each core's timer last fired** - `xhci`'s MSI on core
  2 restarted that core's periodic timer until the watchdog fired (`bdc7adaa`, kernel audit A9-4).

It is Windows' Device Manager in spirit - a list of devices, grouped, with a warning on the ones that
are not right - and it adds the thing Device Manager cannot show: who holds authority over each device
(CLAUDE.md 26.9).

## Principles

1. **Read only, always.** Nothing here enables, disables, rebinds or writes a register. `restart
   <service>` already covers the one useful action, and a utility that only reads can be trusted at a
   glance, which is when it is wanted - while the machine is misbehaving. A read with side effects (a
   status register that clears when read) is left out or marked.
2. **Each owner answers for its own devices; the utility only asks.** No new kernel responsibility. The
   sources are in section 9; one new introspection query is the only kernel change, and it needs the
   operator's go-ahead and a constitution note (section 10).
3. **One record shape for every section.** The shell's pipes carry one table with one set of columns,
   so every row is one device with the same columns (section 3) and any combination of sections pipes.
   On the terminal the rows are grouped under section headings, so it reads like Device Manager.
4. **Honest about absence.** A field its owner cannot answer reads `not available (no driver)` or `not
   reported by this host`, never silently missing. A device nothing drives is listed, with a warning.
5. **The same section names on every board.** A board shows the sections it has: a Pi 2 has no `pci`,
   the Pis have `soc` (fixed SoC blocks granted by device kind), the PCs have none.
6. **ASCII only** (the framebuffer font has no dashes or ellipses), lowercase verbs, and the house
   rules of `utilities/0_conventions.md` - help, version, tab completion, pipes.

## 1. Verbs

| Verb | Kind | What it does |
|---|---|---|
| `hardware` | report, pipes | the overview: one line for the machine, then every section |
| `hardware <section>[,<section>...]` | report, pipes | those sections only: `cpu`, `memory`, `pci`, `usb`, `soc`, `interrupts`, `firmware`, `power` |
| `hardware <device>` | report, pipes as lines | one device in full, in plain words, with who holds authority over it |
| `hardware <device> debug` | report, pipes as lines | the same device as the hardware sees it: registers, IDs, addresses, a snapshot |
| `hardware <section> debug` | report, pipes as lines | debug for every device in the section |
| `hardware problems` | report, pipes | only what is wrong - the warning-icon view |
| `hardware tree` | report, pipes | every device by how it connects, with a `parent` column |
| `hardware why <device>` | report, pipes as lines | why a device is handled the way it is - passthrough, no driver, which service drives it |
| `hardware report [write <path>]` | report | everything, for a bug report: overview, problems, debug for every device, the build |
| `hardware events` | report, pipes | a timeline: attached, removed, driver died, granted, released, reset, faults |
| `hardware compare <report>` | report, pipes | what changed since a saved report |
| `hardware help` / `hardware version` | | the house conventions |

A device is named as the overview names it: a PCI address (`00:10.0`), a USB host and port (`xhci 7`,
`ehci 1.4`), `core N`, `ram`, or a SoC block's kind.

## 2. The overview

```
gsh> hardware
x86-64 - AMD GX-420GI, 4 cores, 7 GiB - IOMMU: AMD-Vi, on

cpu
  DEVICE   KIND        DRIVER   STATE     DETAIL
  core 0   cpu core    kernel   running   boot core; timer periodic, 100 Hz
  core 1   cpu core    kernel   running   timer periodic, 100 Hz
  core 2   cpu core    kernel   idle      timer periodic, 1 Hz while idle
  core 3   cpu core    kernel   running   timer periodic, 100 Hz

memory
  DEVICE   KIND        DRIVER   STATE     DETAIL
  ram      system RAM  kernel   in use    7 GiB total, 11 MiB used

pci
  DEVICE   KIND          DRIVER         STATE      DETAIL
  00:10.0  USB 3 (xHCI)  xhci           running    DMA confined, domain 129; MSI -> core 2
  00:12.0  USB 2 (EHCI)  ehci           running    DMA passthrough
  00:11.0  SATA (AHCI)   block-driver   running    DMA passthrough
  00:01.1  HD audio      audio-driver   running    DMA confined, domain 10; MSI -> core 2
  01:00.0  ethernet      nic-driver     running    DMA passthrough; link down (no cable)
  00:02.6  unknown       -              no driver  vendor 1022, class 0x...

usb
  DEVICE      KIND               DRIVER     STATE    DETAIL
  xhci 7      WiFi (RTL8188CUS)  wifi-usb   joined   0bda:8176, high speed
  ehci 1      hub                ehci       running  4 ports
  ehci 1.2    mass storage       -          no driver
  ehci 1.4    keyboard           ehci       running  046d:c30a, low speed

9 devices with a driver, 2 without - hardware <device> for one in full
```

One section, or several:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware usb
usb
  DEVICE      KIND               DRIVER     STATE    DETAIL
  xhci 7      WiFi (RTL8188CUS)  wifi-usb   joined   0bda:8176, high speed
  ehci 1      hub                ehci       running  4 ports
  ehci 1.2    mass storage       -          no driver
  ehci 1.4    keyboard           ehci       running  046d:c30a, low speed
```

## 3. Pipes: one record shape

Every row of every section is one device with the same columns:

| Column | Meaning |
|---|---|
| `section` | `cpu`, `memory`, `pci`, `usb`, `soc`, ... |
| `device` | the name `hardware <device>` takes |
| `kind` | what it is, in words, with the vendor when known (section 8) |
| `driver` | the service that drives it, `kernel`, or `-` |
| `state` | `running`, `idle`, `joined`, `no driver`, `link down`, ... - the owner's word |
| `detail` | one line of what matters most for this kind |
| `parent` | what it hangs off (`hardware tree`), empty for the top level |

So sections combine and filter like any record source (`docs/records.md`):

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware memory,cpu | where state=idle | select device detail
DEVICE   DETAIL
core 2   timer periodic, 1 Hz while idle

gsh> hardware pci,usb | where driver=-
SECTION  DEVICE    KIND          DRIVER  STATE      DETAIL
pci      00:02.6   unknown       -       no driver  vendor 1022, class 0x...
usb      ehci 1.2  mass storage  -       no driver
```

The labelled-line views (`hardware <device>`, `debug`, `why`) pipe as text, to `match` and `count`, the
way `wifi info` does.

## 4. One device in full, with authority

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware 00:10.0
device     00:10.0
kind       USB 3 host controller (xHCI), class 0x0c0330
id         1022:7914 (AMD)
driver     xhci, on core 2, restarted 485 times
devices    xhci 7 (WiFi, RTL8188CUS)
authority  held by xhci only:
           registers   BAR0 0xfeb68000, 8 KiB, mapped at 0x100000000
           dma         arena 0x34d9000..0x35fd000 (1168 KiB), confined by the IOMMU, domain 129
           interrupt   MSI vector 0x30 -> core 2
           power       none (no power control for this device)
granted by the kernel at spawn, from the supervisor's request for class 0x0c0330 (device confirmed by the kernel's own scan)
```

The `authority` block is the point. A grant in the wrong place is shown as one:

```
authority  held by audio-driver:
           ...
granted for class 0x040300, but the device here is class 0x0c0330 - MISMATCH
```

## 5. `debug`: the device as the hardware sees it

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware 00:10.0 debug
device      00:10.0  (bus 0, device 0x10, function 0)
config      vendor 1022 device 7914 rev 11 class 0c0330 header 00
command     0x0406 (memory on, bus master on, INTx off)
bar0        0xfeb68004 (64-bit memory, 8 KiB)
msi         cap at 0x50, address 0xfee12000, data 0x0030, ctrl 0x0087
            -> vector 0x30, LAPIC id 18 (core 2)
xhci        version 0x0100, 32 slots, 8 ports, context 32 bytes
            USBCMD 0x00000005  USBSTS 0x00000018  CRCR CRR=1
port 7      PORTSC 0x00220e03 (connected, enabled, high speed)
iommu       DTE V=1 TV=1 mode 4, domain 129, root 0x0f3a1000
            arena 0x34d9000..0x35fd000, faults since boot: 3943, last 0x34e5dc0
read now    snapshot at uptime 0d 00:41:07 - registers change; run it again to compare

gsh> hardware cpu debug
core 0      LAPIC id 16, boot core
            timer: periodic, init 62386, current 41120, vector 0x20
            ticks since boot 248113, last tick 4 ms ago
core 2      LAPIC id 18
            timer: periodic, init 6238600 (idle), current 3910233
            ticks since boot 308912, last tick 610 ms ago, 113 halts since
```

Every line names where it came from in the source, not on screen: PCI configuration from
`hw-enumerator`, controller registers from the driver over its own protocol (as `wifi debug transport`
does today), LAPIC and IOMMU state from the kernel (section 10).

## 6. `problems`: the warning-icon view

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware problems
SEVERITY  DEVICE    PROBLEM                      DETAIL
warning   00:10.0   IOMMU faults                 3943 since boot, last 0x34e5dc0 (inside its arena), 41 min ago
warning   00:02.6   no driver                    vendor 1022, class 0x...
warning   ehci 1.2  no driver                    mass storage, behind the hub
notice    01:00.0   link down                    no cable; the radio carries the link
notice    xhci      restarted often              485 restarts this boot (chaos ran)

5 problems: 0 errors, 3 warnings, 2 notices - hardware <device> for one in full
```

`where severity=error` with nothing to show says `(nothing - no errors)`. What the afternoon of
2026-10-08 would have shown:

```
error     00:10.0   controller will not reset    3 attempts failed per start, 300 times since 16:16; reboot to recover
```

Each problem is a loud line some owner already logs; this collects them into one place. It is
`selfcheck`'s complement: `problems` is what is wrong now, `selfcheck hardware` (section 13) is whether
the invariants hold.

## 7. `interrupts`

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware interrupts
SOURCE    VECTOR  KIND      DEVICE / USE          CORE   COUNT     RATE
timer     0x20    LAPIC     scheduler tick        all    1203445   400/s
ipi       0xf0    IPI       cross-core wake       all    88411     12/s
00:10.0   0x30    MSI       xhci                  2      913022    38/s
00:01.1   0x31    MSI       audio-driver          2      40112     1/s
00:12.0   11      INTx      ehci                  0      4410      0/s
01:00.0   5       INTx      nic-driver            -      0         0/s   (link down)

gsh> hardware interrupts | where core=2
```

Placement made visible - which matters more, not less, on a single-core machine.

## 8. Names

A small fixed table built into the image - tens of entries, for the hardware this project runs on, not
the 30,000-line PCI ID database (26.6.1, no heap) - so rows say who made a device: `AMD 7914`,
`Realtek RTL8168`, `Realtek RTL8188CUS (0bda:8176)`, `Logitech (046d:c30a)`. Anything not in it shows
its raw hex ID, never a guess.

## 9. Who answers each field

| Field | Owner | Exists today? |
|---|---|---|
| PCI address, class, vendor, device, BAR, legacy IRQ | `hw-enumerator` (x86, Pi 4, VisionFive) | yes - ops 1, 2, 3 |
| which service drives which device, restarts | the supervisor (its spawn rows, `MANAGED`) | yes, as state; no query yet |
| CPU cores, memory | the kernel, `InspectKernel` | yes - behind `cores` and `mem` |
| USB devices with a driver here | the host drivers' `usbdev` reports | yes |
| USB devices with NO driver here | the host drivers | **no** - the hosts report only what they bound |
| fixed SoC blocks (the Pis) | the kernel, by device kind | yes, as boot lines; no query yet |
| controller registers (`debug`) | the driver, over its own protocol | per driver; `wifi debug transport` is the pattern |
| per-device grant, IOMMU domain and faults, MSI target, timer state | the kernel | **no** - section 10 |
| radio firmware and its load check | the radio drivers | yes, as boot lines |
| the reasons in `why` | fixed strings beside each decision in code | **no** - to be added with each decision |
| events | the `events` service | the service exists; the events do not yet |

## 10. The one kernel change

The per-device facts only the kernel holds - the grant (window, arena, interrupt route, power), the
IOMMU domain and fault count, the MSI target core, and each core's timer state - need one new
`InspectKernel` query, behind the existing `INTROSPECT` capability. A new query on an existing syscall,
not a new syscall, and no new responsibility: the kernel already holds every one of these facts and
logs most of them at spawn. It still widens the surface Commandment I pins, so it needs the operator's
go-ahead and a constitution note before it is written.

## 11. More views

`hardware tree` - by connection:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware tree
machine  x86-64, AMD GX-420GI
|- cpu          4 cores
|- memory       7 GiB
|- pci 00
|  |- 00:10.0   USB 3 (xHCI)        xhci
|  |  `- port 7   WiFi (RTL8188CUS)   wifi-usb   joined
|  |- 00:12.0   USB 2 (EHCI)        ehci
|  |  `- port 1   hub (4 ports)
|  |     |- port 2  mass storage    (no driver)
|  |     `- port 4  keyboard        ehci
|  |- 00:11.0   SATA (AHCI)         block-driver
|  |  `- disk     SSD, 32 GiB       fs  (drives for more)
|  |- 00:01.1   HD audio            audio-driver
|  `- 00:02.6   unknown             (no driver)
`- pci 01
   `- 01:00.0   ethernet (RTL8168)  nic-driver  link down
```

`hardware why <device>` - the policy behind a device, from a short fixed string kept next to the
decision in code, so the explanation cannot drift from the behaviour:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware why 00:12.0
00:12.0 (USB 2, EHCI) runs in IOMMU passthrough, not confined:
  the controller keeps a stale DMA pointer into the firmware's ROM (~0xffffffc0) that survives
  its own reset; confining it turns that harmless read into a fault and kills the keyboard
  (docs/iommu.md 4a)

gsh> hardware why xhci 7
xhci 7 is driven by wifi-usb: xhci bound 0bda:8176 as the radio and the supervisor started
wifi-usb when the host reported it attached (docs/usb-device-drivers.md)
```

`hardware report` - one file to attach to a failed card:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware report write /hw-report.txt
wrote /hw-report.txt - 11 devices, 5 problems, debug for every device, 18 KiB
```

It holds the build and uptime, the overview, `problems`, and `debug` for every device - what CLAUDE.md
19 asks a bug report for, without searching a serial log.

`hardware events` - a timeline, on the `events` service:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware events | last 6
TIME      DEVICE    EVENT                 DETAIL
19:44:33  00:10.0   driver died           xhci, killed (chaos)
19:44:33  00:10.0   released              IOMMU domain 129, table freed
19:44:34  00:10.0   granted               to xhci, confined, domain 129
19:44:34  00:10.0   controller reset      ok, 15 ms
19:44:34  xhci 7    attached              0bda:8176 WiFi, bound to wifi-usb
19:44:37  xhci 7    joined                the radio carries the link
```

`hardware firmware`:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware firmware
DEVICE    FIRMWARE                VERSION   SIZE       CHECK                       LOADED
xhci 7    rtl8192cufw_TMSC.bin    88.2      16094 B    hash verified at load       19:44:34, 1 try
```

On the Pi 4 it lists the CYW43455's image, NVRAM and CLM; on the VisionFive the AIC8800 patches and
`fmacfw`.

`hardware compare <report>` - a regression check across builds:

<!-- doc-command-ok: a mockup of a designed view; only what utilities/58_hardware.md lists is built -->
```
gsh> hardware compare /hw-report-0930.txt
CHANGE    DEVICE    WAS                          NOW
added     xhci 7    -                            WiFi (RTL8188CUS), wifi-usb
changed   00:01.1   DMA passthrough              DMA confined, domain 10
changed   00:10.0   DMA confined, domain 1       DMA confined, domain 129
same      9 devices unchanged
```

`hardware power` - later, when there is a need:

```
(on the Pi 4)
DEVICE    POWER        CONTROL               DETAIL
cpu       1.5 GHz      power service lease   at max while a lease is open; 0 leases now
wifi      on (WL_ON)   DevicePower           cut and restored by wifi radio powercycle
soc       52.0 C       read only             firmware mailbox, read now
```

## 12. Per board

| Board | Sections |
|---|---|
| HP T630, Dell Wyse | cpu, memory, pci, usb, interrupts, firmware |
| Raspberry Pi 4 | cpu, memory, pci (the VL805 behind its PCIe), usb, soc (the SD radio, the PWM jack), interrupts, firmware, power |
| VisionFive 2 Lite | cpu, memory, pci, usb, soc (the radio's SD host), interrupts, firmware |
| Raspberry Pi 2 | cpu, memory, usb (on `dwc2`), soc, interrupts, firmware, power - no pci |

## 13. `selfcheck hardware`

The day's three bug classes as assertions, on every board, every card:

```
gsh> selfcheck hardware
PASS  every driver's device matches the kernel's scan (6 of 6)
PASS  no device is confined to another device's arena (2 confined)
PASS  every core ticked within the last second (4 of 4)
PASS  every confined device has its own IOMMU domain (2 domains)
FAIL  00:10.0 IOMMU faults since boot: 3943 (expected 0)
run: ran 5, failed 1, skipped 0
```

## 14. Build order

1. **From facts that exist, no kernel change:** the overview, sections, `hardware <device>` (without the
   kernel-held authority lines), the names table, `tree`, and `problems` from the owners' own reports.
2. **With the kernel query (section 10), after the operator's go-ahead:** authority in full, `debug`,
   `interrupts`, `report`, `why`.
3. `events`, `firmware`, `compare`, and `selfcheck hardware`.
4. `power` and temperature, when something needs them.

## 15. Out of scope

Anything that changes hardware - enable, disable, rebind, write a register, update firmware. A host
listing USB devices it has no driver for (section 9) is new plumbing in each host and is part of step 1's
`usb` section only as far as the hosts already report; a full inventory is its own change.
