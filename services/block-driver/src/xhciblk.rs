// SPDX-License-Identifier: GPL-2.0-only
//! Reach the USB disk through the **`xhci` SERVICE** instead of the in-kernel USB stack.
//!
//! `usbdisk.rs` reaches its device through four syscalls - `usb_disk_sectors/read/write/flush`.
//! Those syscalls exist for exactly one reason: to expose a USB stack that lives IN THE KERNEL. As
//! long as they are the only route to a disk, `kernel/src/arch/aarch64/xhci.rs` - 2742 lines of ring
//! 0 parsing descriptors and frames supplied by whatever was plugged in - cannot be deleted, and
//! Commandment I stays broken on this port (§4.4: the kernel contains no drivers).
//!
//! This module is the other route. Same four operations, addressed to the `xhci` service by name
//! over IPC, using the block protocol that service already serves (`services/xhci/src/msc.rs`).
//!
//! ## Why this is a proxy and not a rewrite
//!
//! `fs` talks to `block-driver`; `block-driver` talks to the device. Putting the driver in a service
//! adds one hop and changes nothing else: the wire format `fs` sees is untouched, the busy-retry
//! policy stays here, and `block-driver` remains the thing that owns the name "the disk". A design
//! where `fs` talked to `xhci` directly would have been fewer hops and worse - `fs` would learn
//! which bus its storage is on, which is precisely the coupling `block-driver` exists to prevent.
//!
//! ## Failure is a failure, not a fallback
//!
//! If the `xhci` service does not answer, these return failure. They do NOT quietly try the
//! syscalls instead. A silent fallback between two different drivers for the same device is the
//! §26.7 hazard in its purest form: the system would appear to work while the fact that the
//! userspace path is broken went unreported, and the in-kernel driver we are trying to delete would
//! be keeping the lie alive. One reacquire-and-retry is attempted first, because a `None` most
//! often means the service restarted and our cap went stale (§14.3), which is recovery rather than
//! fallback - it re-establishes the SAME path.

use godspeed_sdk::{Message, ServiceContext};

use super::{OP_CAPACITY, OP_FLUSH, OP_READ_BLOCK, OP_WRITE_BLOCK, STATUS_OK};

/// The service that owns the host controller, addressed by name so a restart is transparent (§3.11).
const XHCI: &str = "xhci";

/// One request/reply to `xhci`, with a single reacquire-and-retry.
///
/// The retry exists for one specific, expected condition: the service restarted and this cap went
/// stale. Reacquiring by name re-establishes the same path (§14.3). It is ONE retry, not a loop -
/// a service that is genuinely gone must surface as a failure rather than as an operation that
/// never returns.
fn rpc(ctx: &ServiceContext, req: &[u8]) -> Option<Message> {
    let msg = Message::from_bytes(req);
    if let Some(r) = ctx.request_with_reply(XHCI, &msg) {
        return Some(r);
    }
    let _ = ctx.reacquire_by_name(XHCI);
    ctx.request_with_reply(XHCI, &msg)
}

/// How many times to ask `xhci` for the capacity before coming up with no disk.
///
/// This bound is not decoration. `block-driver` is spawned BEFORE `xhci` (slots 2 and 5 in the boot
/// log), and even once spawned the service has to reset the controller, walk every root port,
/// address the device and read its geometry. Asking once and believing the silence would report "no
/// disk" on every boot of a machine that has one - the failure would look like broken hardware and
/// be perfectly reproducible.
const CAPACITY_ATTEMPTS: u32 = 200;

/// Total addressable sectors, or 0 for "no disk" - the same value the syscall reported, so every
/// caller's no-disk handling is unchanged.
///
/// Waits on the SERVICE'S ANSWER, not on a duration (Commandment VIII): each attempt reacquires by
/// name and yields, so this finishes the moment `xhci` can answer rather than after a fixed sleep
/// chosen to be "probably long enough". The bound is the second half of that rule - a service that
/// never answers is a failure-truth, not a reason to wait forever, so we come up with no disk and
/// say so.
pub fn sectors(ctx: &ServiceContext) -> u64 {
    for attempt in 1..=CAPACITY_ATTEMPTS {
        if let Some(r) = rpc(ctx, &[OP_CAPACITY]) {
            let p = r.payload_bytes();
            if p.len() >= 9 && p[0] == STATUS_OK {
                let n = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
                // A `STATUS_OK` with 0 sectors is an ANSWER - the service is up and reports no disk
                // attached. Retrying past it would spin the full bound on every diskless machine.
                if attempt > 1 {
                    ctx.log_fmt(format_args!(
                        "block-driver: xhci answered on attempt {} - {} sectors", attempt, n));
                }
                return n;
            }
        }
        // Not up yet, or its cap went stale across a restart. Both are recovered the same way.
        let _ = ctx.reacquire_by_name(XHCI);
        ctx.yield_cpu();
    }
    ctx.log("block-driver: xhci service never reported a capacity - NO disk (the service is absent or stuck; storage is unavailable, data on the stick is untouched)");
    0
}

/// Read one 512-byte sector. `false` means the read did not happen - never a partially-filled buf.
pub fn read(ctx: &ServiceContext, lba: u64, buf: &mut [u8; 512]) -> bool {
    let mut req = [0u8; 9];
    req[0] = OP_READ_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    let Some(r) = rpc(ctx, &req) else { return false };
    let p = r.payload_bytes();
    // The length check is not a formality: a short reply with STATUS_OK would otherwise copy
    // whatever the message buffer held into a sector the filesystem then trusts.
    if p.len() < 513 || p[0] != STATUS_OK {
        return false;
    }
    buf.copy_from_slice(&p[1..513]);
    true
}

/// Write one 512-byte sector.
pub fn write(ctx: &ServiceContext, lba: u64, buf: &[u8; 512]) -> bool {
    let mut req = [0u8; 521];
    req[0] = OP_WRITE_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    req[9..521].copy_from_slice(buf);
    let Some(r) = rpc(ctx, &req) else { return false };
    let p = r.payload_bytes();
    !p.is_empty() && p[0] == STATUS_OK
}

/// SYNCHRONIZE CACHE. Its result is RETURNED, because a stick acknowledges a write into its own
/// buffer and only this makes it durable - and because the constitution's crash-recovery guarantee
/// is explicitly conditional on a backend that can be ordered (§6.1, the 2026-07-25 amendment).
pub fn flush(ctx: &ServiceContext) -> bool {
    let Some(r) = rpc(ctx, &[OP_FLUSH]) else { return false };
    let p = r.payload_bytes();
    !p.is_empty() && p[0] == STATUS_OK
}
