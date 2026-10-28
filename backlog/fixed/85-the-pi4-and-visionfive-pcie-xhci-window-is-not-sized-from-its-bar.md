# 85. The Pi 4's and VisionFive's PCIe `xhci` window is a fixed size, not its BAR

**Status: CLOSED 2026-10-28.** Built in `cee3dca0` and carded on both boards on `02a9d3ed`: the Pi 4's
`spawn[mmio]: 'xhci' BAR 0x600000000 ... 4096 bytes, the BAR's own size` (the VL805, where it had 64 KiB),
and the VisionFive's 65536 bytes - from its device-tree `reg`, because that `xhci` is the JH7110's own
Cadence controller and not on PCIe at all, which this entry had wrong. USB worked inside both windows,
and QEMU `virt` showed the probe across three `xhci` respawns. The VisionFive's `nic-driver` (its SoC
Ethernet, also off-bus) kept the fixed window at first; on 2026-10-28, later, it too is sized from its
device-tree `reg` (`net::window_len`), built and QEMU-checked on `virt` for the PCI path, its VisionFive card
owed.

**Previous status: OPEN - FIX BUILT 2026-10-28 (`cee3dca0`), QEMU-verified on riscv64 `virt` (`xhci` 16384 bytes, e1000 131072, each the BAR), awaiting the Pi 4 and VisionFive cards.** The VisionFive's `xhci` turned out NOT to be PCIe: it is the JH7110's Cadence controller, sized from its device-tree `reg`. *Was:* OPEN, 2026-10-13. Moved out of `backlog/fixed/80` (K1) when that entry was closed, because
it was the one part of K1 left open and was recorded only there.

## What is true

`backlog/fixed/80` K1 found that every PCI driver was granted a fixed 64 KiB MMIO window whatever its
BAR's size, so on the T630 the audio grant also covered the HDMI audio controller, `xhci`, EHCI and
AHCI: authority beyond what the grant names (CLAUDE.md 3.1). It was fixed for x86 on 2026-10-09: x86
`pci::bar_len` sizes a memory BAR at grant time, and the window is the BAR (hardware-verified on the
T630: audio 16 KiB, xhci 8 KiB, EHCI 256 bytes, AHCI 1 KiB, the RTL8168 4 KiB).

The other ports answer 0 from `bar_len` and keep the fixed window, and say so at spawn. Two of them
have a PCIe `xhci`: the Pi 4 (VL805) and the VisionFive 2 Lite. On those two the `xhci` grant is
still the fixed window, not the BAR.

## What is ruled out

- Not a reach into another device on either board as far as is known: each has one device behind its
  PCIe root, so a window wider than the BAR covers unassigned space rather than a neighbour. That is
  not checked against each board's address map, and it is the reason this is recorded rather than
  argued away.

## Next step

Implement `bar_len` for the aarch64 and riscv64 PCIe paths (the standard probe: write all ones to the
BAR, read back, restore, under the config lock), as x86 does. A kernel change in `arch/`, so it is
verified on both boards: the spawn line names the window, and `selfcheck` passes before and after a
chaos run.
