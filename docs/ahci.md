# AHCI (SATA) Block Driver

> **Status:** Design doc, non-normative (trails `CLAUDE.md`). Records the AHCI
> backend for `block-driver`, the modern-hardware disk path.

## 1. Why AHCI

The ATA PIO backend (docs/persistence.md §5) works in QEMU's legacy IDE but the
T630's SSD is **AHCI-only** — its firmware exposes no legacy IDE, so a read-only
probe of ports 0x1F0/0x170 returned `status 0xFF` (no drive) on real hardware.
AHCI is the standard SATA interface: it works on the T630 *and* in QEMU
(`ich9-ahci`), so it is the portable, modern path.

AHCI replaces only `block-driver`'s **bottom half** — how sectors move. The block
IPC (`ReadBlock`/`WriteBlock`), the on-disk format, `mkfs`, `fs`, and reboot
survival all live above the block layer and are device-independent.

## 2. Shape of the driver (DMA + MMIO)

Unlike ATA PIO (port I/O), AHCI is **MMIO + DMA**, exactly like the USB drivers:

- The kernel finds the AHCI controller in the PCI scan (class 0x01, subclass
  0x06, progif 0x01), records its **ABAR** (BAR5) + BDF (`pci.rs`), and at spawn
  maps the ABAR (HBA registers) into `block-driver` and grants it a small
  physically-contiguous **DMA arena** (`task/mod.rs`, the same path as xhci/ehci).
  The driver reads the window via `ctx.mmio()` and the arena via `ctx.dma_region()`.
- Per-port DMA structures the driver builds in its arena: a **command list** (32
  command headers), a **received-FIS** area, and per-command a **command table**
  (a command FIS + a **PRDT** — physical region descriptors pointing at the data
  buffer). A transfer = build a Register-Host-to-Device FIS (READ/WRITE DMA EXT),
  point the PRDT at the data buffer, set the command-issue bit, wait for
  completion, check the task-file for errors.

## 3. IOMMU (H1)

AHCI is DMA-capable, so on a machine with an IOMMU it should be **confined** to
its arena like xhci (§6.4). The T630 has a working IOMMU. The driver is brought up
in **passthrough** first (get it correct), then confined once all its controller
DMA is provably inside the granted arena — the same "earned confinement" rule the
USB drivers follow (docs/iommu.md).

## 4. Build steps (incremental, against QEMU `ich9-ahci`)

Developed behind the `block-driver/ahci` cargo feature so the ATA PIO tests stay
green during the migration; becomes the default (retiring ATA PIO + the IDE probe)
once read/write/fs/reboot are verified on it. Test: `osdev test blockdev-ahci`.

- **Step A — detect + HBA init. ✅ done.** Map ABAR, enable AHCI mode (GHC.AE),
  read CAP/VS/PI, enumerate implemented ports, report which carry a SATA disk
  (DET=3, sig 0x00000101). Verified: 6 ports, disks on ports 0/1.
- **Step B — port init + IDENTIFY. ✅ done** (`osdev test blockdev-ahci`, AHCI.B).
  Stop the port (clear ST/FRE, wait CR/FR), plant the command list + received-FIS
  base in the arena, restart (FRE then ST), then issue IDENTIFY DEVICE via a command
  header + H2D Register FIS + single-PRDT command table, wait on PxCI, and parse the
  result. Verified: model `QEMU HARDDISK`, 131072 sectors (64 MiB). The full DMA
  command path (command list / FIS / PRDT / command-issue / completion) now works.
- **Step C — read. ✅ done** (`osdev test blockdev-ahci`, AHCI.C). READ DMA EXT (0x25)
  via `issue()` into the PRDT data buffer; block IPC `ReadBlock` restored. Verified:
  `fs` mounts the AHCI disk over IPC (`fs: mounted`).
- **Step D — write + integrate. ✅ done** (AHCI.D). WRITE DMA EXT (0x35) + FLUSH EXT
  (0xEA); the block-IPC serve loop runs in the AHCI backend; `fs` spawns and the full
  file round-trip works over AHCI (`fs: file round-trip OK`). The whole filesystem
  stack now runs on AHCI. Harness: boot on legacy IDE, the persist disk ALONE on
  `ich9-ahci` (→ port 0), mirroring the T630 (SSD is the only SATA disk).
- **Step E — confine.** Bring AHCI under IOMMU confinement (§6.4) once its DMA is
  fully arena-resident; then make `ahci` the default backend.

## 5. Register cheat-sheet

HBA (from ABAR): `CAP` 0x00, `GHC` 0x04 (bit31 AE), `IS` 0x08, `PI` 0x0C, `VS` 0x10.
Port *n* (base 0x100 + n·0x80): `PxCLB` 0x00, `PxFB` 0x08, `PxIS` 0x10, `PxIE` 0x14,
`PxCMD` 0x18 (bit0 ST, bit4 FRE, bit14 FR, bit15 CR), `PxTFD` 0x20, `PxSIG` 0x24,
`PxSSTS` 0x28 (DET in 3:0), `PxSERR` 0x30, `PxCI` 0x38 (command-issue bitmask).
