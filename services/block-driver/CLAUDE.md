# services/block-driver/

Userspace **ATA PIO** disk driver (persistence, v2; §6.3, `docs/persistence.md`).
Currently a TCB member (§6.1); the v2 goal is to drop it (§6.3, Phase 3).

## Device: ATA PIO, no DMA

The driver talks to a legacy IDE disk on the **secondary channel** (command block
`0x170-0x177`, control `0x376`) using programmed I/O only — no DMA, no MMIO. Because
a PIO driver never points a device at RAM, it has **no DMA-anywhere reach**: it is
least-privilege *by construction* and does not need IOMMU confinement (the H1 problem
does not apply). This is also what makes it a clean candidate to leave the TCB (§6.3),
independent of IOMMU presence. See `docs/persistence.md` §5.

## Why not virtio-blk

virtio-blk is a QEMU-only paravirtual device that runs on no real hardware, and its
virtqueue is DMA. ATA PIO works in QEMU **and** on real hardware (legacy/IDE mode), is
far simpler, and is the conceptual stepping-stone to a future AHCI driver. (The earlier
virtio plan was reconsidered before any code — `docs/persistence.md` §2–§5.)

## Capability: `hw_pio` (kernel-mediated port I/O)

Ring-3 services cannot execute `in`/`out`, and granting IOPL would be ambient authority
over every port (§3.1). So port I/O is kernel-mediated: the driver's `hw_pio` grant (its
contract port ranges) is recorded at spawn, and the `PortRead`/`PortWrite` syscalls
validate **every** access against it. The SDK `Pio` wrapper (`Pio::read16` etc.) hides
the syscalls so the driver stays `unsafe`-free (§18). The grant store + validation live
in `kernel/src/capability/hw_pio.rs` (a permitted unsafe layer, §18.5).

```toml
[capabilities]
hw_pio = ["0x170+0x8", "0x376+0x1"]   # ATA secondary channel
```

## Phases (docs/persistence.md §10)

- **Phase 1 (done):** read sector 0 and log it — proves the cap-mediated port-I/O path.
  Verified by `osdev test blockdev` (boots with a disk on the ATA secondary channel,
  the driver reads back the host-written magic in sector 0).
- **Phase 2+:** the block read/write interface to `fs` over IPC (Read/Write blocks),
  then transactional recovery toward dropping block-driver from the TCB (§6.3).

## Exposed interface (to fs via IPC — later phases)

| Request   | Args                        | Response |
|-----------|-----------------------------|----------|
| `Read`    | LBA (u64), block count (u32)| data bytes or `IoError` |
| `Write`   | LBA (u64), data bytes       | `Ok` or `IoError` |

## Failure semantics (§6.2)

While still a TCB member (Phase 1–2), block-driver death = kernel panic = system reboot.
Phase 3 (§6.3) makes it restartable and drops it from the TCB.
