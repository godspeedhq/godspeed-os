// SPDX-License-Identifier: GPL-2.0-only
//! `block-driver` ARM backend: a USB mass-storage stick, served through the in-kernel DWC2 stack.
//!
//! **Why this backend exists.** The Pi has exactly one SD slot and the machine boots from it, so
//! formatting that card destroys the boot medium (`fs` refuses to, see `foreign_disk`). A USB stick is
//! the storage that can actually be given to GSFS on this board: boot from SD, store on USB.
//!
//! **Why it goes through the kernel.** ARM does not route device IRQs to userspace yet, so the whole USB
//! stack - controller, enumeration, Bulk-Only transport - lives in the kernel (`arch/arm/dwc2.rs`,
//! `arch/arm/CLAUDE.md`). This driver therefore does not touch hardware at all: it holds the `USB_DISK`
//! capability and moves blocks with three gated syscalls, exactly as the ARM `nic-driver` bridges USB
//! ethernet frames to `net-stack`. The block protocol above them - the same one `fs` speaks to the AHCI
//! and EMMC backends - is this driver's own, so `fs` cannot tell which disk it is talking to.
//!
//! **Core affinity.** The kernel serves these syscalls only from core 0, because the DWC2 DMA buffer is
//! shared with the keyboard poll that runs in core 0's timer ISR, kept mutually exclusive by an ARM
//! syscall running with interrupts masked (the soundness invariant on `dwc2::DMA`). A request from
//! another core is refused, so the supervisor spawns this driver on core 0. Storage and the keyboard no
//! longer share a HOST CHANNEL - each stream has its own (`CH_BULK`/`CH_KBD`) - so what is shared is the
//! one DMA scratch buffer, not the channel state.
//!
//! **Busy is not failure.** A stick NAKs while its flash is occupied. The kernel bounds how long it will
//! hold the core waiting and then answers `-2` (busy) rather than `-1` (failed), and the waiting happens
//! HERE, between yields, where interrupts are on and every other task still runs.

use godspeed_sdk::{Message, ServiceContext};

/// Re-ask while the device says BUSY, yielding in between.
///
/// A NAK is the device asking us to come back, not a failure - and the kernel now says so with `-2`
/// instead of folding it into `-1`. The waiting belongs HERE rather than in the syscall: between
/// attempts this task simply yields, so interrupts are enabled, the timer tick runs, the keyboard is
/// polled and every other service runs. Inside the syscall the same wait costs a stalled core, which
/// is why it was bounded to 5 ms there and why a device that stayed busy longer got declared broken.
///
/// Bounded (§26.6) by attempts, and each attempt waits on TRUTH - the transfer completing - rather
/// than on a clock (Commandment VIII).
/// How many times to re-ask a busy device before calling it a failure.
///
/// 6000 attempts at a 5 ms core-hold each is roughly **30 seconds**, which is deliberately the same
/// order as the USB mass-storage command timeout Linux uses. The previous 200 was about one second,
/// and hardware said plainly that this was too short: 36 blocks in a single run reported
/// `gave up after 200 busy retries - the device stayed busy, it did not fail`, and `fs` then degraded
/// a mount over a device that was alive and simply working. A stick doing internal garbage collection
/// or a block remap can hold off for seconds; one second is not a storage timeout, it is a guess.
///
/// This does not risk hanging on a DEAD device: a device that has gone answers with transaction errors
/// or stops answering EP0 entirely, and both are detected separately and immediately (`XACT_ERR_MAX`,
/// and the revival path). "Busy" is positive evidence the device is present and responding - waiting
/// for it is waiting on truth, and the bound here only stops that wait being unbounded (§26.6).
///
/// Cost when it does happen: this task yields between attempts, so the wait costs nothing but its own
/// latency - interrupts stay on, the timer runs, every other service runs.
const BUSY_RETRIES: u32 = 6_000;
fn with_busy_retry(ctx: &ServiceContext, what: &str, lba: u64, mut op: impl FnMut() -> i64) -> bool {
    for n in 0..BUSY_RETRIES {
        match op() {
            0 => return true,
            -2 => { ctx.yield_cpu(); }   // busy: hand the CPU on, then ask again - expected, silent
            _ => return false,           // a real error; the kernel has already named it
        }
        let _ = n;
    }
    // RUNNING OUT is a real failure and must say so. Individual busy hand-backs are silent because
    // they are the expected case, but that silence was applied to this path too - so a genuine
    // give-up surfaced as `fs: block write failed ... (device I/O error)` with nothing anywhere
    // explaining why, which is precisely the unexplained failure §26.7 exists to prevent. The count
    // is the useful fact: it says the device was ALIVE and asking us to wait, for this long, and we
    // stopped - which is a different problem from a device that is broken, and has a different fix.
    ctx.log_fmt(format_args!(
        "block-driver: {} lba {} gave up after {} busy retries - the device stayed busy, it did not fail",
        what, lba, BUSY_RETRIES));
    false
}

/// Serve one block-IPC request. Same wire protocol as the AHCI and EMMC backends - `fs` is unaware of
/// which one it is talking to.
fn serve(sectors: u64, ctx: &ServiceContext, p: &[u8], reply: godspeed_sdk::CapHandle) {
    use super::{OP_CAPACITY, OP_FLUSH, OP_READ_BLOCK, OP_WRITE_BLOCK, OP_WRITE_ZEROS, STATUS_ERR, STATUS_OK};
    let err = |ctx: &ServiceContext| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[STATUS_ERR])); };
    if p.is_empty() { return err(ctx); }
    if p[0] == OP_FLUSH {
        // The one backend that genuinely needs this: a stick acknowledges a WRITE(10) into its own
        // buffer, so without SYNCHRONIZE CACHE a reset loses the tail of everything just written.
        let status = if ctx.usb_disk_flush() { STATUS_OK } else { STATUS_ERR };
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        return;
    }
    if p[0] == OP_CAPACITY {
        let mut out = [0u8; 9];
        out[0] = STATUS_OK;
        out[1..9].copy_from_slice(&sectors.to_le_bytes());
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
        return;
    }
    if p.len() < 9 { return err(ctx); }
    let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    match p[0] {
        OP_READ_BLOCK => {
            let mut buf = [0u8; 512];
            if with_busy_retry(ctx, "read", lba, || ctx.usb_disk_read_status(lba, &mut buf)) {
                let mut out = [0u8; 513];
                out[0] = STATUS_OK;
                out[1..].copy_from_slice(&buf);
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
            } else { err(ctx); }
        }
        OP_WRITE_BLOCK => {
            if p.len() < 521 { return err(ctx); }
            let mut buf = [0u8; 512];
            buf.copy_from_slice(&p[9..521]);
            let status = if with_busy_retry(ctx, "write", lba, || ctx.usb_disk_write_status(lba, &buf)) { STATUS_OK } else { STATUS_ERR };
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        }
        OP_WRITE_ZEROS => {
            if p.len() < 17 { return err(ctx); }
            let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
            let zero = [0u8; 512];
            let mut ok = true;
            for i in 0..count {
                if !with_busy_retry(ctx, "write-zeros", lba + i, || ctx.usb_disk_write_status(lba + i, &zero)) { ok = false; break; }
            }
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[if ok { STATUS_OK } else { STATUS_ERR }]));
        }
        _ => err(ctx),
    }
}

/// Serve block I/O from the USB mass-storage device. The caller has already confirmed one is attached.
pub fn run(ctx: &ServiceContext, sectors: u64) -> ! {
    ctx.log_fmt(format_args!("block-driver: USB mass storage serving block I/O ({} sectors = {} MiB)",
                             sectors, sectors / 2048));
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        serve(sectors, ctx, msg.payload_bytes(), reply);
        ctx.remove_cap(reply);
    }
}
