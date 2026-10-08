<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `hardware` - what this machine is, and what drives each part of it

Implementation shape: **shell built-in**, asking each device's owner - the kernel's introspection for
the cores and memory, `hw-enumerator` for the PCI bus, and the supervisor (`supcmd::DEVICES`) for which
service it runs for which device. Read only, always. The design, including everything not built yet,
is `docs/hardware-design.md`.

## Status, as built and honest (2026-10-08)

**Step 1 of the design's build order is built: the facts that already exist, no kernel change.** Run in
QEMU (`osdev test shell`). Not yet run on hardware.

Built: the overview, the sections `cpu`, `memory`, `pci`, `soc`, `display` and `usb`, a comma list of
them, one device in full, the vendor and class names, and records when piped. **Not built**, each
answering `designed, not built yet` rather than being mistaken for a device: `debug`, `problems`,
`interrupts`, `tree`, `why`, `report`, `events`, `firmware`, `compare` and `power`. One device in full
says the kernel's grant - its authority block - is not shown yet: that needs the introspection query
of the design's section 10.

Two limits of step 1, stated: the `usb` section lists only the devices the supervisor starts a driver
for (today the USB WiFi dongle) - a keyboard or a stick a host drives itself, or a device nothing
drives, is not reported by its host yet; and a USB device is named by its IDs (`0bda:8176`), not by
its host and port.

## 1. Verbs

| Verb | Kind | What it does |
|---|---|---|
| `hardware` | report, pipes | a line for the machine, then every section this machine has |
| `hardware <section>[,<section>...]` | report, pipes | those sections; one this machine lacks says so |
| `hardware <device>` | report, pipes as lines | one device in full, named as `hardware` names it |
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
  00:10.0     USB 3 (xHCI)         xhci           running     AMD 1022:7914, IRQ 11
  00:02.6     class 0x......       -              no driver   AMD 1022:...., IRQ ...
  ...

9 device(s) with a driver, 2 without - hardware <device> for one in full
```

## 3. Pipes (rule 12)

Every section is one record shape - `section`, `device`, `kind`, `driver`, `state`, `detail` - so any
combination pipes as one table: `hardware cpu,memory | count`, `hardware | where driver=-`,
`hardware pci | select device kind`. One device is labelled lines and pipes as text, to `match`.

## 4. Failure says whose answer is missing

The supervisor not answering leaves the DRIVER column at `-` and the overview says so in a line of its
own; `hw-enumerator` not answering means no `pci` section. An unknown name answers `no section or
device '<name>'` and fails. A word of the design not built yet answers that it is not built yet and
fails, so `if hardware problems` cannot be read as an answer.

## 5. Tab completion (rule 9)

The built sections complete; a device name is the machine's, typed. No word completes to a path.
