// SPDX-License-Identifier: GPL-2.0-only
//! `block-driver` USB backend: a USB mass-storage stick, reached over IPC through the USB host SERVICE
//! (`xhciblk`: `dwc2` on the Pi 2, `xhci` on the Pi 4 and the VisionFive 2).
//!
//! **Why this backend exists.** The Pi has exactly one SD slot and the machine boots from it, so
//! formatting that card destroys the boot medium (`fs` refuses to, see `foreign_disk`). A USB stick is
//! the storage that can actually be given to GSFS on this board: boot from SD, store on USB.
//!
//! **Why it does not touch the hardware.** The USB stack - controller, enumeration, Bulk-Only transport
//! - is the host SERVICE (`services/dwc2` on the Pi 2, `services/xhci` elsewhere). On arm32 it used to be in the kernel, because ARM did not route
//! device IRQs to userspace; it does now (`USB_VECTOR`, `arch/arm/irq.rs`), and `kernel/src/arch/arm/dwc2.rs`
//! is deleted. This driver therefore moves blocks over IPC to that service (`xhciblk`), exactly
//! as the ARM `nic-driver` bridges USB ethernet frames to `net-stack`. The block protocol above them - the same one `fs` speaks to the AHCI
//! backend - is this driver's own, so `fs` cannot tell which disk it is talking to.
//!
//! **Core affinity: none any more.** This used to say the kernel served its `usb_disk_*` syscalls only
//! from core 0, because the in-kernel DWC2 stack shared one DMA buffer with the keyboard poll in core 0's
//! timer ISR. Both the syscalls and that stack are gone; there is no syscall left on this path.
//!
//! **Busy is not failure, and the waiting is the host's.** A stick NAKs while its flash is occupied.
//! When the stack was in the kernel its syscall answered BUSY and this driver waited between yields;
//! the service path answers only done or failed, because the host service's Bulk-Only layer waits a slow
//! device out itself. The retry loop that waited here was unreachable and is gone (`request`).

use godspeed as gs;
use godspeed_sdk::ServiceContext;

/// One block request to the USB host service, and its failure said.
///
/// This was `with_busy_retry`: a loop of up to 6000 attempts bounded by a 30 s clock, with BUSY and
/// ABSENT arms, written for the in-kernel USB stack whose syscall answered "busy" while a stick's flash
/// was occupied. That stack is gone, and the service path answers only done or failed (`dev_read`,
/// `dev_write` below) - the waiting already happens inside the host's Bulk-Only layer - so the loop always
/// ended on its first attempt and the two arms were never reached (`backlog/80` V9). What is left is
/// what it really did.
fn request(ctx: &ServiceContext, what: &str, lba: u64, mut op: impl FnMut() -> i64) -> bool {
    match op() {
        0 => true,
        code => {
            // WHO refused matters: this failure comes from the USB host SERVICE, not the kernel, and
            // naming the wrong one sends an operator to read the wrong log. The code is always -1:
            // `xhciblk` already logged why (no answer, or the reacquire failed).
            ctx.log_fmt(format_args!(
                "block-driver: {} lba {} refused by the {} service, status {}",
                what, lba, crate::xhciblk::XHCI, code));
            false
        }
    }
}


// --- Which USB stack backs this disk -----------------------------------------------------------
//
// ONE route now: the USB host SERVICE, by IPC (`xhciblk`). There used to be two, chosen at BUILD
// time and never at runtime - the in-kernel stack by syscall, or the `xhci` SERVICE by IPC - never
// mixed, with no fallback between them: a silent switch from the userspace driver to the kernel one
// would hide exactly the failure this port exists to eliminate (§26.7), and would keep alive the
// in-kernel stack that Commandment I says must go. Both in-kernel stacks, and the build flags that
// chose between the routes, are deleted (CLAUDE.md 6.4, amendments 2026-08-09 and 2026-08-17).

// 0 = done, anything else an error. The service path never reports busy, and that is correct rather
// than a gap: the BOT layer inside `xhci` already waits a slow device out (its transfer budget is
// generous precisely because the old 2 s one aborted healthy commands). By the time it answers, the
// waiting has happened - so a failure here is a real I/O error.
fn dev_read(ctx: &ServiceContext, lba: u64, buf: &mut [u8; 512]) -> i64 {
    if super::xhciblk::read(ctx, lba, buf) { 0 } else { -1 }
}

fn dev_write(ctx: &ServiceContext, lba: u64, buf: &[u8; 512]) -> i64 {
    if super::xhciblk::write(ctx, lba, buf) { 0 } else { -1 }
}

fn dev_flush(ctx: &ServiceContext) -> bool { super::xhciblk::flush(ctx) }

/// Serve one block-IPC request. Same wire protocol as the AHCI backend - `fs` is unaware of which one
/// it is talking to.
fn serve(sectors: u64, ctx: &ServiceContext, p: &[u8], reply: crate::Reply) {
    use super::{OP_CAPACITY, OP_FLUSH, OP_READ_BLOCK, OP_WRITE_BLOCK, OP_WRITE_ZEROS, STATUS_ERR, STATUS_OK};
    let err = |ctx: &ServiceContext| { reply.send(ctx, &[STATUS_ERR]); };
    if p.is_empty() { return err(ctx); }
    if p[0] == OP_FLUSH {
        // The one backend that genuinely needs this: a stick acknowledges a WRITE(10) into its own
        // buffer, so without SYNCHRONIZE CACHE a reset loses the tail of everything just written.
        let status = if dev_flush(ctx) { STATUS_OK } else { STATUS_ERR };
        reply.send(ctx, &[status]);
        return;
    }
    if p[0] == OP_CAPACITY {
        // Ask the DEVICE. `sectors` (the startup count) is deliberately NOT used here.
        //
        // It was the second copy of one fact: `xhci` knows what is attached, and this held a snapshot
        // of what it said at boot. Commandment III allows a derived view only while it is reconciled
        // with its source, and nothing ever reconciled this one - so `drives` reported a size for a
        // stick that had been unplugged.
        //
        // Reconciled AT THE CALL SITE, which is the only repair path guaranteed to run: a repair path
        // invoked somewhere else may never be invoked at all. An earlier attempt at this was reverted
        // because it appeared to cost input latency - that turned out to be logging and a pessimistic
        // probe timeout, both since fixed, and a capacity query is not a hot path (mount, and `drives`).
        // AN UNREACHABLE PEER IS AN ERROR, NOT A CAPACITY OF ZERO. This sent STATUS_OK with 0
        // sectors whatever the reason, so "xhci is restarting" arrived at `fs` as "the volume is
        // empty" - indistinguishable from an unplugged stick, and acted on as one. `fs` mounts
        // against nothing, and the client that was mid-file-operation fails.
        //
        // STATUS_ERR is what `fs` already retries; a zero is what it believes.
        match super::xhciblk::sectors_now(ctx) {
            super::xhciblk::Capacity::Sectors(sectors) => {
                let mut out = [0u8; 9];
                out[0] = STATUS_OK;
                out[1..9].copy_from_slice(&sectors.to_le_bytes());
                reply.send(ctx, &out);
            }
            super::xhciblk::Capacity::Unreachable => reply.send(ctx, &[STATUS_ERR]),
        }
        return;
    }
    if p.len() < 9 { return err(ctx); }
    let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    match p[0] {
        OP_READ_BLOCK => {
            let mut buf = [0u8; 512];
            if request(ctx, "read", lba, || dev_read(ctx, lba, &mut buf)) {
                let mut out = [0u8; 513];
                out[0] = STATUS_OK;
                out[1..].copy_from_slice(&buf);
                reply.send(ctx, &out);
            } else { err(ctx); }
        }
        OP_WRITE_BLOCK => {
            if p.len() < 521 { return err(ctx); }
            let mut buf = [0u8; 512];
            buf.copy_from_slice(&p[9..521]);
            let status = if request(ctx, "write", lba, || dev_write(ctx, lba, &buf)) { STATUS_OK } else { STATUS_ERR };
            reply.send(ctx, &[status]);
        }
        OP_WRITE_ZEROS => {
            if p.len() < 17 { return err(ctx); }
            let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
            let zero = [0u8; 512];
            let mut ok = true;
            for i in 0..count {
                if !request(ctx, "write-zeros", lba + i, || dev_write(ctx, lba + i, &zero)) { ok = false; break; }
            }
            reply.send(ctx, &[if ok { STATUS_OK } else { STATUS_ERR }]);
        }
        _ => err(ctx),
    }
}

/// Serve block I/O from the USB mass-storage device. `sectors` is the startup count, for the log line
/// only - it may be 0 (no stick yet, or the host not answering), and capacity is re-asked per request.
pub fn run(ctx: &ServiceContext, sectors: u64) -> ! {
    ctx.log_fmt(format_args!("block-driver: USB mass storage serving block I/O ({} sectors = {} MiB)",
                             sectors, sectors / 2048));
    loop {
        let msg = gs::ipc::recv(ctx);
        let cap = match gs::ipc::take_sent_cap(ctx) { Some(c) => c, None => continue };
        // The correlation tag is byte 0 of every request; the backend below never sees it and parses
        // exactly as it always did. Splitting it off HERE, once, is why fourteen reply sites did not
        // each have to learn about it.
        let p = msg.payload_bytes();
        let (tag, body) = match p.split_first() {
            Some((t, rest)) => (*t, rest),
            None => (0, &p[..0]),
        };
        serve(sectors, ctx, body, crate::Reply::plain(cap, tag));
        gs::cap::remove(ctx, cap);
    }
}
