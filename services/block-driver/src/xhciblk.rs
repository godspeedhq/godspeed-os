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
//! The syscall route in `usbdisk.rs` survives for a board with no such service, and **no shipping
//! port is one**: arm32's DWC2 stack left the kernel a week after aarch64's xHCI did (CLAUDE.md §6.4,
//! amendment 2026-08-17), so every USB board now comes through here.
//!
//! Which service to ask is `STORAGE_HOST`, set by `build.rs` from the target. It used to be a build
//! FEATURE, which was a footgun - the switch had to reach three crates by hand, and setting only some
//! gave two drivers on one controller, or none. A value DERIVED from the target cargo is already
//! building for cannot be half-set, which is the property that was actually wanted.
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

/// The USB host-controller SERVICE that owns the disk, by name - so a restart is transparent (§3.11).
///
/// Different service, identical protocol: `xhci` drives the Pi 4's VL805 and the VisionFive 2's
/// Cadence core, `dwc2` drives the Pi 2's DesignWare core. The wire format is byte-for-byte the same,
/// which is the whole reason this client needed no porting - only the name it asks for. That name is
/// one entry in `build.rs`'s board table rather than a `cfg` here, so a new board states its host
/// once, beside the rest of what makes it a board.
///
/// `"none"` on a machine whose disk is not on USB at all (x86: AHCI over PCI). Nothing reaches this
/// module there - the AHCI backend takes the call and never returns - and a name that resolves to
/// nothing fails loudly rather than reaching some other service by accident.
pub(crate) const XHCI: &str = env!("STORAGE_HOST");

/// One request/reply to `xhci`, with a single reacquire-and-retry.
///
/// The retry exists for one specific, expected condition: the service restarted and this cap went
/// stale. Reacquiring by name re-establishes the same path (§14.3). It is ONE retry, not a loop -
/// a service that is genuinely gone must surface as a failure rather than as an operation that
/// never returns.
/// How long ONE question to the USB host service may take. Two numbers, because the two kinds of
/// question have nothing in common:
///
///   * a CAPACITY query is answered out of state the service already holds, in microseconds, so a
///     live peer never needs seconds. The bound exists for the peer that will never answer.
///   * a READ or WRITE crosses BOT/SCSI to real media and legitimately takes time.
///
/// Both stay under the 30 s `fs` allows this service, so a stuck peer is reported HERE - by the
/// service that knows which peer - rather than surfacing as `fs` timing out on us.
const CAPACITY_RPC_SECS: i64 = 2;
const IO_RPC_SECS: i64 = 10;

/// One request to the USB host service, BOUNDED.
///
/// This used `request_with_reply`, whose own SDK comment says it plainly: "No deadline on this
/// variant, so `None` is always a lost peer, never a timeout." An unbounded `call` wakes on a reply
/// or on the replier's DEATH (§8.6) - and an idling peer is neither. Measured 2026-09-20 on riscv64
/// with no USB controller attached: `xhci` comes up, logs `no controller MMIO granted - idling`,
/// receives this request and never replies, and this service blocked here FOREVER - before
/// `usbdisk::run`, so it served nothing, answered nothing, and logged nothing. `fs` then ate a 30 s
/// timeout per request and never reached `serving file API`.
///
/// **That is the rule above all the others broken: a dependency that is missing, dead or silent must
/// RETURN with a loud "unavailable", never hang.** The x86 side of this crate already gets it right -
/// no AHCI controller means `serve_no_disk` answers capacity with a truthful zero - and `main.rs`
/// carries a long comment about fixing exactly this defect once before, on that path. It was fixed
/// there and not here because nothing had ever booted a `storage_is_usb` board with no USB host.
///
/// Note what the bound does NOT fix, so nobody reads more into it: `sectors()` already wrapped this
/// in a 20 s deadline loop, and that bound was INERT, because a bound around a call that never
/// returns is never evaluated. An outer deadline cannot rescue an unbounded inner call.
fn rpc_within(ctx: &ServiceContext, req: &[u8], secs: i64) -> Option<Message> {
    let msg = Message::from_bytes(req);
    match ctx.request_with_reply_call_err(XHCI, &msg, secs) {
        Ok(Some(r)) => return Some(r),
        Ok(None) => {
            // THE DEADLINE PASSED, AND THIS IS NOT RETRIED. The request may still be in flight, so a
            // second one would leave the first reply to arrive as an orphan and desync every exchange
            // after it - the same reason `fs` refuses to re-send a request we did not answer in time.
            // Retry belongs to `Err` alone, which means the SEND failed and nothing is outstanding.
            ctx.log_fmt(format_args!(
                "block-driver: '{}' did not answer within {} s - reporting storage UNAVAILABLE rather \
                 than waiting on it (it is reachable but silent: busy, wedged, or idling with no \
                 controller)", XHCI, secs));
            return None;
        }
        Err(_) => {}   // the SEND failed: no request is outstanding, so a retry is safe
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
    let out = match ctx.request_with_reply_call_err(XHCI, &msg, secs) {
        Ok(v)  => v,
        Err(_) => None,
    };
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

/// The I/O-shaped wrapper. Reads, writes and flushes cross to real media; capacity does not, and
/// asks for `CAPACITY_RPC_SECS` explicitly at its two call sites.
fn rpc(ctx: &ServiceContext, req: &[u8]) -> Option<Message> {
    rpc_within(ctx, req, IO_RPC_SECS)
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
        if let Some(r) = rpc_within(ctx, &[OP_CAPACITY], CAPACITY_RPC_SECS) {
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

/// What asking `xhci` for the capacity actually produced.
///
/// **"MY PEER IS RESTARTING" IS NOT "THERE IS NO DISK", and collapsing the two took storage down on
/// the Pi 4.** The hotplug crashed `xhci`; the kernel cleared its name correctly and re-registered it
/// 440 ms later, also correctly. `block-driver` asked 215 ms into that window, got "the name does not
/// resolve" - true, and transient - and published `no USB storage stick - NO disk`, serving 0 sectors.
/// `fs` mounted against nothing and the running selfcheck died fifteen seconds later. Every name gap
/// in that run closed: 440, 323, 246, 470, 455, 168, 285, 527 ms. Not one failed.
///
/// So the three zeros this function used to return are not one answer. `Sectors(0)` is `xhci` SAYING
/// the bay is empty, which is a fact worth publishing. `Unreachable` is `xhci` saying nothing at all,
/// which is a fact about the PEER and says nothing whatever about the hardware.
///
/// The fix is deliberately NOT "remember the last capacity". That is a derived view outliving its
/// source, it is what made `drives` report 15267 MiB for a stick unplugged minutes earlier, and
/// §26.4/§14.3 both forbid it. The answer is to report a transient failure AS one and let the caller
/// retry - which `fs` already does, and which is the same discipline the reply-orphan fix follows.
pub enum Capacity {
    /// `xhci` answered: this many sectors. Zero means it has no device bound, which is an answer.
    Sectors(u64),
    /// `xhci` said NOTHING - restarting, or its cap went stale across a restart. NOT a verdict about
    /// the disk, and must never be published as one.
    ///
    /// Only silence qualifies. Any reply, however terse, is an answer from the peer and belongs in
    /// `Sectors` - mapping a short reply here made a diskless boot retry forever instead of coming up
    /// with no disk.
    Unreachable,
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
pub fn sectors_now(ctx: &ServiceContext) -> Capacity {
    // ZERO HAS THREE CAUSES AND THEY NEEDED TELLING APART. A post-chaos Pi 2 served
    // `storage unavailable` for 23 s across two selfcheck runs while `dwc2` demonstrably HAD the
    // stick - it had just enumerated it and read sector 0 back as "GSFS". Everything above this
    // reported the same word for all three cases, so the log said "no capacity" and could not say
    // whether the driver was unreachable, refusing, or honestly reporting an empty bay.
    //
    // Logged only on the ZERO paths, so a healthy mount stays silent and a stuck one explains itself
    // on the first request rather than after another hardware round (§26.7).
    let Some(r) = rpc_within(ctx, &[OP_CAPACITY], CAPACITY_RPC_SECS) else {
        // UNREACHABLE, not empty. See `Capacity` - this is the case that took storage down.
        ctx.log("block-driver: the USB host service did not ANSWER (restarting, or its cap went stale) - reporting storage UNAVAILABLE, not 'no disk'");
        return Capacity::Unreachable;
    };
    let p = r.payload_bytes();
    if p.len() < 9 {
        // A SHORT REPLY IS STILL A REPLY. This was briefly mapped to `Unreachable`, on the reasoning
        // that a reply which does not parse tells you nothing - and it broke a diskless boot outright:
        // `fs` got STATUS_ERR instead of "0 sectors", treated it as transient exactly as intended, and
        // retried forever without ever reaching `serving file API`.
        //
        // The peer ANSWERED. On both ARM ports a one-byte reply is `[STATUS_ERR]`, which the USB host
        // service sends when it has no mass-storage device bound - so this is an empty bay reported
        // tersely, not a protocol mismatch, and the old comment saying otherwise was wrong. Publishing
        // zero lets a diskless machine come up diskless, which is the correct outcome and the one it
        // had before.
        ctx.log_fmt(format_args!(
            "block-driver: capacity 0 - the USB host service replied {} byte(s), not a capacity: it is reachable with no disk bound", p.len()));
        return Capacity::Sectors(0);
    }
    if p[0] != STATUS_OK {
        ctx.log_fmt(format_args!(
            "block-driver: capacity 0 - the USB host service REFUSED (status {}) - it is reachable but has no disk bound", p[0]));
        return Capacity::Sectors(0);
    }
    let n = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    if n == 0 {
        // THE ONE ZERO THAT IS AN ANSWER: `xhci` is up, it replied, and it has nothing bound. That is
        // a fact about the hardware and is published as one.
        ctx.log("block-driver: capacity 0 - the USB host service ANSWERED zero: no mass-storage device bound");
    }
    Capacity::Sectors(n)
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
