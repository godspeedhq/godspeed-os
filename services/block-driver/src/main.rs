// SPDX-License-Identifier: GPL-2.0-only
//! `block-driver` - userspace **AHCI (SATA)** disk driver (persistence, v2; §6.3,
//! docs/ahci.md, docs/persistence.md).
//!
//! An MMIO + DMA driver: the kernel maps the AHCI HBA's ABAR and grants a
//! physically-contiguous DMA arena at spawn (the same path as the USB drivers).
//! It IDENTIFYs the disk, runs a boot self-test, then serves block read/write to
//! `fs` over IPC.
//!
//! (ATA PIO + the `hw_pio` capability were the bring-up backend; retired once AHCI
//! proved out - the T630's SSD is AHCI-only, so AHCI is the production path.)

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Backend by architecture: x86 talks AHCI (SATA, MMIO+DMA); ARM (Raspberry Pi 2) talks the BCM2835 EMMC
// (Arasan SDHCI, PIO). Both satisfy the same block-IPC protocol below and are reached through the
// kernel-granted MMIO window (`ctx.mmio()`).
#[cfg(not(target_arch = "arm"))]
mod ahci;
#[cfg(target_arch = "arm")]
mod sdhci;

// Block IPC protocol (fs <-> block-driver). MUST match `services/fs`.
//   Request : [op:u8, lba:u64 LE, (WriteBlock only: 512 data bytes)]
//   Reply   : [status:u8, (ReadBlock only: 512 data bytes)]
// The LBA is u64 (persistence §6.3): GSFS capacity fields are u64, so the block
// address reaches the device at full width.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
// Capacity request: [OP_CAPACITY] → reply [STATUS_OK, sectors:u64 LE]. Lets `fs`
// size a freshly-flashed filesystem to the real disk (drives §7, persistence §6.3).
const OP_CAPACITY: u8 = 3;
// Write-zeros: [OP_WRITE_ZEROS, lba:u64, count:u64] → zero `count` blocks from `lba`,
// batched into multi-sector AHCI commands (no per-block IPC, no data carried). `fs` uses
// it to zero the free bitmap at format time so `drives flash` stays fast on a big disk.
const OP_WRITE_ZEROS: u8 = 4;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// Run the arch-appropriate backend against the kernel-granted MMIO window.
#[cfg(not(target_arch = "arm"))]
fn backend_run(ctx: &ServiceContext, m: &godspeed_sdk::Mmio) -> ! { ahci::run(ctx, m) }
#[cfg(target_arch = "arm")]
fn backend_run(ctx: &ServiceContext, m: &godspeed_sdk::Mmio) -> ! { sdhci::run(ctx, m) }

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    match ctx.mmio() {
        Some(m) => backend_run(&ctx, &m),
        None => {
            ctx.log("block-driver: no AHCI controller found (no SATA disk?)");
            // DRAIN our IPC endpoint forever, never just yield: a registered service that idles here without
            // recv'ing lets a flood (or any stray send) fill its 16-deep queue and sit at 16/16 FOREVER - the
            // flood-endpoint disease (the logger stub / xhci idle()). recv() parks between messages, so the
            // core still idles. We POLL (try_recv) rather than block on recv: a cross-core flood that must
            // WAKE a deeply-blocked recv on an AP is unreliable under QEMU TCG (the drain flaked in the
            // flood-storm pin); the self-driven poll drains every quantum with no wake needed. Pinned by the
            // shell-test `chaos flood-storm block-driver` step (QEMU's pc machine has no AHCI, so it sits here).
            loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
        }
    }
}
