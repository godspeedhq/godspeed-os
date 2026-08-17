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

// Backend by architecture: x86 talks AHCI (SATA, MMIO+DMA); ARM (Raspberry Pi 2) storage is a USB stick
// (`usbdisk`, through the in-kernel DWC2 stack). Both satisfy the same block-IPC protocol below.
//
// The BCM2835 EMMC / Arasan SDHCI backend (`sdhci.rs`) is DELIBERATELY NOT COMPILED IN. On the Pi 2 the
// EMMC IS the SD card the board boots from - firmware + kernel + FAT boot partition - and using it as
// GSFS storage writes over the boot partition and corrupts the card to RAW (it destroyed two boot
// cards). There is no safe way to use a single-slot Pi's boot card as storage, so the backend is a
// hazard, not a fallback. The file is kept for reference (a future board with a SEPARATE storage medium
// could use it), but it is not a module here so it cannot be reached - see `backend_run`.
/// A reply cap plus the CORRELATION TAG of the request it answers.
///
/// Every reply to `fs` carries the tag of the request that asked for it, so a reply that arrives out
/// of order is DETECTABLE rather than silently believed. Chaos proved the need: after 98 rounds of
/// killing and restarting these services the block protocol came back out of step, `fs` read LBA 7702
/// and got a 17-byte reply where a 513-byte block belonged, and the only reason it noticed was that
/// the SHAPE happened to be wrong. Two reads of different blocks are both 513 bytes and shape cannot
/// separate them at all - one would simply be believed, which is silent data corruption.
///
/// The tag lives in the reply HANDLE rather than being remembered separately, so it cannot be
/// forgotten: there is no way to answer a request without it, and the compiler says so.
#[derive(Clone, Copy)]
pub struct Reply {
    pub cap: godspeed_sdk::CapHandle,
    pub tag: u8,
}

impl Reply {
    /// Answer, with the tag in front. `body` is the reply exactly as it was before tagging.
    pub fn send(&self, ctx: &godspeed_sdk::ServiceContext, body: &[u8]) {
        // 513 is the largest body (status + one block); one more for the tag. Fixed, on the stack,
        // no allocation (§26.6.1).
        let mut out = [0u8; 514];
        out[0] = self.tag;
        let n = body.len().min(out.len() - 1);
        out[1..1 + n].copy_from_slice(&body[..n]);
        let _ = ctx.send_by_handle(self.cap, &godspeed_sdk::Message::from_bytes(&out[..1 + n]));
    }
}

#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
mod ahci;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
mod usbdisk;
// Now on BOTH ARM targets: arm32 reaches the `dwc2` service and aarch64 the `xhci` service, through
// this one client. The wire format is identical, so only the service name differs.
mod xhciblk;

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
// Flush: [OP_FLUSH] → make every previously acknowledged write durable on the medium, reply
// [STATUS_OK] or [STATUS_ERR]. A write completing does NOT mean the bytes reached the disk - a
// USB stick acknowledges into its own volatile buffer - so `fs` asks for durability explicitly at
// the points where it promises it (format, journal commit). Backends that cannot flush say so
// once at startup rather than quietly implying the data is safe (§26.7).
const OP_FLUSH: u8 = 5;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// Run the arch-appropriate backend against the kernel-granted MMIO window.
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
fn backend_run(ctx: &ServiceContext, m: &godspeed_sdk::Mmio) -> ! { ahci::run(ctx, m) }
/// ARM storage is the USB stick, and ONLY the USB stick. Never the SD/EMMC card.
///
/// **Both ARM ports, for the same reason.** The Pi 4 has the same single-slot topology as the Pi 2:
/// one SD card, which is the boot medium. The rule below was established on the Pi 2 and applies
/// unchanged there.
///
/// The Pi 2 has one SD slot and boots from it: that card carries the firmware, the kernel image, and a
/// FAT boot partition. It is the boot medium, full stop - there is no safe way to also hand it to GSFS,
/// because GSFS's superblock lives at LBA 0, exactly where the card's partition table is. Reaching the
/// card through the SD/EMMC backend let `fs` write GSFS over the boot partition and **corrupt the card
/// to RAW - observed destroying two boot cards** (the `foreign_disk` guard is not enough: once any
/// GSFS-looking bytes are on the card, or after a single `drives flash ... force` aimed at the wrong
/// disk, `fs` mounts it and writes). So the SD backend is not a fallback; it is a hazard, and it is
/// removed. With no USB stick, there is simply NO storage - exactly what x86 reports with no disk
/// attached - and `fs` comes up storage-unavailable. `usbdisk::run` with a 0 sector count serves that
/// no-disk state (capacity 0, every read/write refused) WITHOUT touching the card.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn backend_run(ctx: &ServiceContext) -> ! {
    // Where the sector count comes from is the same build-time choice usbdisk.rs documents: the
    // in-kernel stack by syscall, or the `xhci` service by IPC. No probe, no fallback.
    // arm32 now asks the `dwc2` SERVICE over IPC, exactly as aarch64 asks `xhci` - the in-kernel
    // stack's `usb_disk_*` syscalls are no longer the path. Same client, same wire format, different
    // service name (`xhciblk::XHCI`), which is why this is a cfg flip rather than a port.
    // ONE quick question at startup, not a 20 s wait for an answer we no longer use.
    //
    // This called `sectors()`, which blocks up to 20 s for a capacity. That made every boot with no
    // stick in it stall for 20 s before the first `drives` could answer - the reported "substantial
    // delay on startup, subsequent ones are quick".
    //
    // The wait existed to capture a boot-time sector count. Since `serve()` re-derives capacity on
    // every request, that number no longer answers anything: it is used for the startup log line and
    // then shadowed. So the wait buys nothing and costs 20 s on exactly the machine that has the
    // least to wait for.
    //
    // A stick present but still enumerating now reports 0 here and mounts slightly later, on the
    // first request, through the same self-heal that already recovers a stick plugged in after boot -
    // hardware-proven across several plug/unplug cycles. Trading a guaranteed 20 s stall for a
    // recovery path that is exercised constantly is the right way round.
    let sectors = xhciblk::sectors_now(&ctx);
    if sectors == 0 {
        ctx.log("block-driver: no USB storage stick - NO disk (the SD card is the boot medium and is never written)");
    }
    usbdisk::run(ctx, sectors)
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // On the ARM ports the backend needs NO MMIO: the USB stack is in the kernel and the disk is
    // reached through syscalls. Going through the `ctx.mmio()` gate would refuse a perfectly good USB
    // stick on any board that does not also hand the service a peripheral window it never reads - which
    // is the Pi 4 exactly. So those ports do not ask.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    backend_run(&ctx);
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
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
            // ANSWER while draining. The loop here used to be
            //     loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
            // which retired every request and replied to none - so `fs` blocked forever in its first
            // `block_capacity()`, never reached its own storage-unavailable degraded path, and never
            // printed `fs: serving file API`; every file command in the shell hung behind it. On any
            // machine with no AHCI (an NVMe-only box, QEMU's default `pc`) that is the whole storage
            // stack silently dead, on a branch whose comment says the two services "come up and idle
            // gracefully".
            //
            // The correct version was already in this crate: `ahci::serve_no_disk` answers CAPACITY
            // with a truthful zero and everything else with STATUS_ERR. The drain reasoning below it
            // still holds - what was missing was the reply.
            //
            // It keeps polling rather than blocking on `recv` for the reason the original comment
            // gives (a cross-core flood must not depend on waking a deeply-blocked recv), so the
            // answer path is inlined here rather than delegating to the blocking version.
            loop {
                while let Some(msg) = ctx.try_recv() {
                    let reply = match ctx.take_pending_cap() {
                        Some(c) => c,
                        None => continue,   // nothing to answer on; dropping is all that is left
                    };
                    let raw = msg.payload_bytes();
                    let (tag, p) = match raw.split_first() {
                        Some((t, rest)) => (*t, rest),
                        None => (0, &raw[..0]),
                    };
                    let reply = Reply { cap: reply, tag };
                    let mut out = [0u8; 9];
                    let n = if !p.is_empty() && p[0] == OP_CAPACITY {
                        // Capacity is [STATUS_OK, sectors u64 LE]; zero sectors is the truth here and
                        // is exactly what `fs` reads as "genuinely no disk".
                        out[0] = STATUS_OK;
                        9
                    } else {
                        out[0] = STATUS_ERR;
                        1
                    };
                    reply.send(ctx, &out[..n]);
                    ctx.remove_cap(reply.cap);
                }
                ctx.yield_cpu();
            }
        }
    }
}
