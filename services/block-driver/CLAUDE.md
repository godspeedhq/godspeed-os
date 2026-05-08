# services/block-driver/

virtio-blk userspace driver. TCB member in v1 (§6.1). **Non-restartable in v1.**

## Why it's in the TCB (v1)

`fs` depends on block-driver for all I/O. A restart of block-driver would leave fs in an undefined state mid-operation. v2 goal: give block-driver a clean restart protocol so fs can reconnect (§6.3).

## Capabilities required

- `hw_mmio`: the virtio-blk MMIO region (QEMU default: `0x10001000+0x1000`).
- `hw_interrupt`: IRQ 11 (QEMU default for virtio-blk).
- `ipc_receive ["block-driver"]`: receives read/write requests from fs.

## Exposed interface (to fs via IPC)

| Request   | Args                        | Response |
|-----------|-----------------------------|----------|
| `Read`    | LBA (u64), block count (u32)| data bytes or `IoError` |
| `Write`   | LBA (u64), data bytes       | `Ok` or `IoError` |

## v1 target device

QEMU virtio-blk MMIO, single virtqueue, polling-with-interrupt completion. No DMA in v1.

## Failure semantics (§6.2)

Block-driver death = kernel panic = system reboot (same as other TCB services).
