// SPDX-License-Identifier: GPL-2.0-only
//! Reach the USB disk through the **`xhci` SERVICE** instead of the in-kernel USB stack.
//!
//! `usbdisk.rs` reaches its device through four syscalls - `usb_disk_sectors/read/write/flush` -
//! which exist to expose a USB stack that lives IN THE KERNEL. That was true on aarch64 when this
//! module was written; it is not now. `kernel/src/arch/aarch64/xhci.rs` (2742 lines of ring-0 code
//! parsing descriptors supplied by whatever was plugged in) was DELETED, along with the feature flags
//! that used to select between the two drivers, so on this port the service below is the only route
//! and Commandment I is closed (CLAUDE.md §6.4, amendment 2026-08-09).
//!
//! The syscall route survives for ARM32 (Pi 2), which has no PCIe and no device-IRQ routing to
//! userspace, so its DWC2 stack remains in the kernel. `usbdisk.rs` picks between the two on
//! `cfg(target_arch)` - it used to be a build FEATURE, which was a footgun: the switch had to reach
//! three crates and setting only some gave two drivers on one controller, or none.
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
/// The USB host-controller SERVICE that owns the disk, by name.
///
/// Different service, identical protocol. On AArch64 that is `xhci` driving the Pi 4's VL805; on
/// arm32 it is `dwc2` driving the Pi 2's DesignWare core. The wire format is byte-for-byte the same,
/// which is the whole reason this client needed no porting - only the name it asks for.
#[cfg(target_arch = "arm")]
const XHCI: &str = "dwc2";
#[cfg(not(target_arch = "arm"))]
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
    // WHEN BOTH ATTEMPTS FAIL, SAY WHETHER THE REACQUIRE WORKED. That is the one distinction left
    // between the two causes this path can have, and they need opposite fixes:
    //   reacquired, still silent -> the service is ALIVE but not answering (busy, or wedged)
    //   reacquire FAILED          -> the name did not resolve; there is no live instance to reach
    // A post-chaos Pi 2 sat in this state for 23 s across two selfcheck runs (99 file failures each)
    // while dwc2 demonstrably held the disk, so "no answer" alone was not enough to act on.
    //
    // Logged only when the RETRY also fails, so an ordinary stale-cap recovery - which is the common
    // case and works - stays silent.
    let reacquired = ctx.reacquire_by_name(XHCI);
    let out = ctx.request_with_reply(XHCI, &msg);
    if out.is_none() {
        ctx.log_fmt(format_args!(
            "block-driver: '{}' did not answer, and the retry after reacquire {} - {}",
            XHCI,
            if reacquired { "reacquired OK" } else { "COULD NOT REACQUIRE" },
            if reacquired { "the service is alive but silent (busy or wedged)" }
            else { "the name does not resolve: no live instance" }));
    }
    out
}

/// How long to wait for `xhci` to report a capacity - a REAL DURATION, in milliseconds.
///
/// This was `CAPACITY_ATTEMPTS = 200`, and the Pi 4 showed exactly what is wrong with that:
///
///     10:01:36.708  block-driver: xhci service never reported a capacity - NO disk
///     10:01:40.848  xhci: USB disk ready - 31266816 sectors of 512 B (15267 MiB)
///
/// It gave up after ~0.2 s on something that takes ~4.3 s. Each "attempt" is a failed IPC plus a
/// reacquire plus a yield, and when the peer is not answering those complete almost instantly - so
/// 200 attempts measured how fast the loop spins, not how long the disk gets. A COUNT IS NOT A
/// DURATION, and this is the fourth time that has bitten this port.
///
/// 20 s covers a USB stick enumerating behind a hub with room to spare, and it is bound by the
/// CLOCK, so it means the same thing on any board.
const CAPACITY_TIMEOUT_MS: u64 = 20_000;

/// Total addressable sectors, or 0 for "no disk" - the same value the syscall reported, so every
/// caller's no-disk handling is unchanged.
///
/// Waits on the SERVICE'S ANSWER, not on a duration (Commandment VIII): each attempt reacquires by
/// name and yields, so this finishes the moment `xhci` can answer rather than after a fixed sleep
/// chosen to be "probably long enough". The bound is the second half of that rule - a service that
/// never answers is a failure-truth, not a reason to wait forever, so we come up with no disk and
/// say so.
pub fn sectors(ctx: &ServiceContext) -> u64 {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(CAPACITY_TIMEOUT_MS));
    let mut attempt = 0u32;
    loop {
        attempt += 1;
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
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            break;
        }
    }
    ctx.log("block-driver: xhci service never reported a capacity within 20s - NO disk (the service is absent or stuck; storage is unavailable, data on the stick is untouched)");
    0
}

/// Ask `xhci` for the capacity RIGHT NOW - one attempt, no waiting for enumeration.
///
/// Deliberately not `sectors()`. That one waits up to 20 s because it runs at startup, when the
/// controller legitimately has not finished enumerating; using it to answer an interactive `drives`
/// would hang the shell for 20 s on a machine with no stick in it.
///
/// This exists because `drives` reported 15267 MiB for a stick that had been unplugged minutes
/// earlier: `block-driver` captured the sector count once at startup and served that number forever,
/// so removing the device changed nothing anything above could see. That is a derived view outliving
/// its source, which §26.4 and §14.3 both forbid - the disk is `xhci`'s truth, and a cached copy of
/// another service's truth must be re-derived, not remembered.
pub fn sectors_now(ctx: &ServiceContext) -> u64 {
    // ZERO HAS THREE CAUSES AND THEY NEEDED TELLING APART. A post-chaos Pi 2 served
    // `storage unavailable` for 23 s across two selfcheck runs while `dwc2` demonstrably HAD the
    // stick - it had just enumerated it and read sector 0 back as "GSFS". Everything above this
    // reported the same word for all three cases, so the log said "no capacity" and could not say
    // whether the driver was unreachable, refusing, or honestly reporting an empty bay.
    //
    // Logged only on the ZERO paths, so a healthy mount stays silent and a stuck one explains itself
    // on the first request rather than after another hardware round (§26.7).
    let Some(r) = rpc(ctx, &[OP_CAPACITY]) else {
        ctx.log("block-driver: capacity 0 - the USB host service did not ANSWER (cap stale after its restart, or it is busy)");
        return 0;
    };
    let p = r.payload_bytes();
    if p.len() < 9 {
        ctx.log_fmt(format_args!(
            "block-driver: capacity 0 - short reply, {} bytes (want 9) - protocol mismatch, not an empty bay", p.len()));
        return 0;
    }
    if p[0] != STATUS_OK {
        ctx.log_fmt(format_args!(
            "block-driver: capacity 0 - the USB host service REFUSED (status {}) - it is reachable but has no disk bound", p[0]));
        return 0;
    }
    let n = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    if n == 0 {
        ctx.log("block-driver: capacity 0 - the USB host service ANSWERED zero: no mass-storage device bound");
    }
    n
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
