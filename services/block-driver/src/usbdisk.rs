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
//! **Busy is not failure.** A stick NAKs while its flash is occupied. When the stack was in the kernel,
//! its syscall bounded how long it held the core and then answered BUSY rather than failed, and the
//! waiting happened HERE, between yields (`with_busy_retry`). (Note 2026-10-09: the service path never
//! answers BUSY or ABSENT - `dev_read`/`dev_write` return 0 or -1, see below - so today those two arms
//! of `with_busy_retry` are not reached, and the waiting happens inside the host service's BOT layer.)

use godspeed as gs;
use godspeed_sdk::{ServiceContext, USB_DISK_BUSY, USB_DISK_ABSENT};

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

/// How long a busy device may hold us off before we call it a failure - **the budget that actually
/// means what it says**.
///
/// `BUSY_RETRIES` above is a COUNT, and a count is not a duration. It bounded the wait to ~30 s only
/// because of an accident of the arm32 backend: DWC2 polls inside the syscall for about 5 ms per
/// attempt, so 6000 of them happened to add up to the intended half-minute. The aarch64 backend
/// returns BUSY immediately - correctly, it has nothing to wait on there - and the identical loop then
/// burned its whole budget in **173 ms**, measured on the board. The device was never given time to
/// finish, and a stick doing a block remap was declared broken a fifth of a second in.
///
/// That is the same trap as the chaos harness's `PACE_YIELDS`, where a yield count was read as a
/// duration: a proxy for time that holds on one arch and silently means something else on the next.
/// The tell is a bound whose real value depends on how fast the loop happens to run.
///
/// So the wait is bounded by the CLOCK, and the attempt count stays only as a runaway backstop.
const BUSY_BUDGET_SECS: i64 = 30;

/// Attempts to spend spinning before switching to a paced poll.
///
/// A device that is momentarily busy answers within a handful of attempts, and for that case a yield
/// is exactly right - no sleep latency on the common path. Past this, the device is genuinely working
/// (a remap, or garbage collection) and asking again 34,000 times a second neither helps it nor leaves
/// this core free. One millisecond between attempts is still far finer than the device's own timescale.
const SPIN_ATTEMPTS: u32 = 64;
const BUSY_POLL_MS: u64 = 1;
/// Seconds a single request may be re-asked before the operator is told it is still going.
///
/// A healthy request is sub-millisecond, so crossing this means the device really is holding us off.
/// Short enough to land well before anyone starts wondering, long enough that ordinary slow writes on
/// this stick stay silent.
const SLOW_ANNOUNCE_SECS: i64 = 2;

fn with_busy_retry(ctx: &ServiceContext, what: &str, lba: u64, mut op: impl FnMut() -> i64) -> bool {
    // WAITING MUST BE VISIBLE. Every layer here is behaving correctly when a stick goes slow - the
    // driver waits out one command rather than re-issuing it (which wedges this device), and the retry
    // budget bounds that at ~30 s. But it says NOTHING while it happens, and the whole machine has
    // nothing else to report either, so a correct 30-second wait is indistinguishable from a dead board.
    // That cost three chaos runs: the operator reached for the power switch, twice cut a healthy machine
    // mid-recovery, and the third time only waited because the second had taught them to.
    //
    // A bounded wait that cannot say it is waiting is a silent one (§26.7). Two lines - one when it
    // becomes slow, one when it resolves - turn "the Pi has hung" into "the stick is busy, it is
    // working on it". No behaviour changes; only the silence does.
    // The clock is a SYSCALL, and this is the hottest loop in the storage path - so it is read lazily
    // and sparsely. The first version read it unconditionally at entry, which put an extra syscall on
    // EVERY block read and write (the overwhelming majority of which complete on the first attempt and
    // never wait at all), and then once per busy iteration - thousands per slow request. Chaos pauses
    // went from 2.3% of rounds to 20%: instrumentation added to explain a delay became a cause of it.
    //
    // So: no clock at all unless a request actually goes BUSY, and then only every `CLOCK_SAMPLE_EVERY`
    // attempts. Sampling coarsely costs at most that many attempts of lateness on a 2 s threshold, which
    // is nothing, and takes the cost from thousands of syscalls to ~90 across a full 6000-attempt budget.
    const CLOCK_SAMPLE_EVERY: u32 = 64;
    let mut t0: Option<i64> = None;
    let mut announced = false;
    for n in 0..BUSY_RETRIES {
        match op() {
            0 => {
                // Only speak on the way out if we spoke on the way in - a normal request stays silent
                // (and pays nothing), and a slow one is closed out rather than left unresolved.
                if let (true, Some(start)) = (announced, t0) {
                    ctx.log_fmt(format_args!(
                        "block-driver: {} lba {} completed after {}s - the device was busy, not broken",
                        what, lba, gs::task::epoch_secs_monotonic(ctx) - start));
                }
                return true;
            }
            // Named, not a literal: BUSY must stay outside the capability-error range, or a driver
            // missing its USB_DISK cap lands here and gets retried 6000 times before being reported
            // as a device that "stayed busy" - an authority failure wearing an I/O failure's name.
            USB_DISK_BUSY => {
                // Expected and silent - until it has gone on long enough that silence is itself
                // misleading. Said ONCE per request, not per attempt, and costing a clock read only
                // every CLOCK_SAMPLE_EVERY attempts (see above - this loop cannot afford a syscall).
                if !announced && n % CLOCK_SAMPLE_EVERY == 0 {
                    let now = gs::task::epoch_secs_monotonic(ctx);
                    match t0 {
                        // First time we have actually had to wait: start the clock here, not at entry.
                        None => t0 = Some(now),
                        Some(start) if now - start >= SLOW_ANNOUNCE_SECS => {
                            announced = true;
                            ctx.log_fmt(format_args!(
                                "block-driver: {} lba {} - device busy for {}s and still working; waiting (this is not a hang)",
                                what, lba, now - start));
                        }
                        _ => {}
                    }
                }
                // Pace the retry, then check the wait against the CLOCK rather than the attempt count.
                //
                // Yielding alone is not a wait: it returns immediately when nothing else is runnable,
                // which is how the whole budget went by in 173 ms on aarch64. Spin briefly for the
                // momentarily-busy case, then poll at 1 kHz so the device gets real time and this core
                // is genuinely free in between.
                if n < SPIN_ATTEMPTS {
                    gs::task::yield_now(ctx);
                } else {
                    gs::task::sleep_ms(ctx, BUSY_POLL_MS);
                    if let Some(start) = t0 {
                        if gs::task::epoch_secs_monotonic(ctx) - start >= BUSY_BUDGET_SECS {
                            break; // the budget is a duration, and it is spent
                        }
                    }
                }
            }
            // NOTHING THERE. Not the same as busy, and the difference is the whole point of the code:
            // waiting is only ever right when the device is present and asking for time. Against an
            // empty socket the retry budget bought nothing and cost everything - ~30 s per block, every
            // block of every request, while the kernel logged the refusal and the operator watched a
            // shell that looked hung. One `ls` with the stick out produced over 20,000 refusals.
            //
            // Only a hot-plug can change this answer, and that is not something a loop can wait for, so
            // fail immediately and let the caller decide. `fs` already knows how to come back: it
            // re-mounts on a later request once the device returns.
            USB_DISK_ABSENT => {
                ctx.log_fmt(format_args!(
                    "block-driver: {} lba {} - no USB disk attached (unplugged?); not retrying until one is",
                    what, lba));
                return false;
            }
            // The CODE is the diagnosis, so it is printed rather than folded into a bare failure. A
            // hardware run showed `fs` reporting I/O errors while the kernel's transport log said
            // nothing - the failure was a silent refusal, and this reply was the one place that saw
            // which one (-1 = kernel-internal refusal or transport failure, -2 = capability not held,
            // which is an authority bug, not a device problem). "The kernel has already named it"
            // was only true of transport failures.
            code => {
                ctx.log_fmt(format_args!(
                    // WHO refused matters: this failure comes from the USB host SERVICE, not the
                    // kernel, and naming the wrong one sends an operator to read the wrong log. It
                    // said "refused by kernel" on the Pi 4's first userspace-USB boot, where the
                    // kernel was not in the path at all. Today the code is always -1: `xhciblk`
                    // already logged why (no answer, or the reacquire failed).
                    "block-driver: {} lba {} refused by the {} service, status {}",
                    what, lba, crate::xhciblk::XHCI, code));
                return false;
            }
        }
        let _ = n;
    }
    // RUNNING OUT is a real failure and must say so. Individual busy hand-backs are silent because
    // they are the expected case, but that silence was applied to this path too - so a genuine
    // give-up surfaced as `fs: block write failed ... (device I/O error)` with nothing anywhere
    // explaining why, which is precisely the unexplained failure §26.7 exists to prevent. The count
    // is the useful fact: it says the device was ALIVE and asking us to wait, for this long, and we
    // stopped - which is a different problem from a device that is broken, and has a different fix.
    // Report the ELAPSED TIME, not the attempt count. The count was the misleading number all along:
    // "gave up after 6000 busy retries" reads like half a minute of patience and was a sixth of a
    // second, and nothing in the line said which. Seconds are the fact an operator can act on.
    let waited = t0.map(|s| gs::task::epoch_secs_monotonic(ctx) - s).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "block-driver: {} lba {} gave up after {}s busy - the device stayed busy, it did not fail",
        what, lba, waited));
    false
}


// --- Which USB stack backs this disk -----------------------------------------------------------
//
// ONE route now: the USB host SERVICE, by IPC (`xhciblk`). There used to be two, chosen at BUILD
// time and never at runtime - the in-kernel stack by syscall, or the `xhci` SERVICE by IPC - never
// mixed, with no fallback between them: a silent switch from the userspace driver to the kernel one
// would hide exactly the failure this port exists to eliminate (§26.7), and would keep alive the
// in-kernel stack that Commandment I says must go. Both in-kernel stacks, and the build flags that
// chose between the routes, are deleted (CLAUDE.md 6.4, amendments 2026-08-09 and 2026-08-17).

// The return is the TRI-STATE i64 the old syscall returned (0 = done, USB_DISK_BUSY,
// USB_DISK_ABSENT, other = error), not a bool, because `with_busy_retry` acts differently on each and flattening them would
// turn "the stick is thinking" into "the read failed".
//
// The service path never reports BUSY, and that is correct rather than a gap: the BOT layer inside
// `xhci` already waits a slow device out (its transfer budget is generous precisely because the old
// 2 s one aborted healthy commands). By the time it answers, the waiting has happened - so a failure
// here is a real I/O error, and returning BUSY would send this loop off to wait another 30 s for a
// device that already gave its answer.
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
            if with_busy_retry(ctx, "read", lba, || dev_read(ctx, lba, &mut buf)) {
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
            let status = if with_busy_retry(ctx, "write", lba, || dev_write(ctx, lba, &buf)) { STATUS_OK } else { STATUS_ERR };
            reply.send(ctx, &[status]);
        }
        OP_WRITE_ZEROS => {
            if p.len() < 17 { return err(ctx); }
            let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
            let zero = [0u8; 512];
            let mut ok = true;
            for i in 0..count {
                if !with_busy_retry(ctx, "write-zeros", lba + i, || dev_write(ctx, lba + i, &zero)) { ok = false; break; }
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
