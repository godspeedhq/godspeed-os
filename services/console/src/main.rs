// SPDX-License-Identifier: GPL-2.0-only
//! `console` - the terminal. Owns the display; renders what every other service writes to the console.
//!
//! This is where `kernel/src/fbcon` went (`docs/console-service.md` §9). The kernel used to interpret
//! ANSI escapes, decode UTF-8, keep a character grid and scroll it - a display driver in ring 0, which
//! §4.4 forbids by name and `scripts/commandments.py` had been failing the build over. The terminal is
//! policy, so it is a service; what the kernel kept is a boot/panic blit that cannot be a service
//! because a panic halts every core and so cannot ask one for help.
//!
//! ## How bytes get here
//!
//! Ordinary IPC, deliberately. `ConsoleWrite` (syscall 23) writes serial synchronously - serial remains
//! the source of truth and a captured log is unchanged - and then enqueues the same bytes to this
//! service's endpoint. So there is no new syscall, no new capability, no polling loop and no second ring
//! buffer in the kernel: the transport is the bounded queue and the blocking `recv` that already exist,
//! and this service simply blocks on its endpoint like every other one.
//!
//! **A full queue blocks the WRITER, not the kernel.** The writing task is recorded as a blocked sender
//! and parked until this service drains - ordinary bounded-queue back-pressure (§8.5, §8.6), the same
//! thing any userspace `send` to a full endpoint does. The kernel itself never waits on this service and
//! never depends on it.
//!
//! It dropped instead, at first, on the argument that the kernel must not block - which is true and is
//! preserved above, but the conclusion did not follow. A 16-deep queue cannot hold a thirty-write burst
//! however fast this service renders, so a full-screen `observe now` lost its tail EVERY time, and no
//! amount of rendering speed could have fixed it. A producer has no business running ahead of the
//! display it is writing to.
//!
//! If this service is dead rather than merely behind, the write is not shown and the kernel says so
//! periodically; serial still has every byte.
//!
//! ## Restartability
//!
//! A fresh instance re-maps the framebuffer, clears it, and renders from the next byte on. Scrollback is
//! lost, because scrollback lived in the dead instance's grid - a re-init, not a resume (§14.2). The
//! screen is therefore blank rather than stale, which is the honest state after a restart.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, ServiceContext};

mod render;
mod term;

use term::Term;

/// Request byte: report terminal geometry. The reply is `[rows_lo, rows_hi, cols_lo, cols_hi]`.
///
/// The shell asks for this instead of the kernel (`InspectKernel` query 9 is deleted). Terminal
/// geometry is derived from the safe-area inset, the cell size and the font-scale rule, all of which
/// live here - so this is the only party that can answer, and there is one source of truth for it.
const REQ_DIMS: u8 = 1;

/// Report progress every this many rendered messages.
///
/// Present because the first hardware boot left a question plain observation could not settle: the
/// terminal's queue sat full at 16/16 while it reported ~0% CPU, which is the signature of BOTH "too
/// slow to keep up" and "not running at all", and those need opposite fixes. A count that climbs says
/// the first; a count that stops says the second (§26.7 - measure, do not guess).
const RENDER_REPORT: u64 = 500;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // The framebuffer grant. Without one there is no display to own - which is the normal case on a
    // machine with no framebuffer, and on the Pi 4 until its mapping is made non-cacheable. Say so and
    // park: a console with nothing to render is not a failure, but it must not pretend to work.
    let Some(fb) = ctx.framebuffer() else {
        ctx.log("console: no framebuffer grant - nothing to render, parking");
        loop {
            let _ = ctx.recv();
        }
    };

    ctx.log_fmt(format_args!(
        "console: framebuffer {}x{} {}bpp pitch {}",
        fb.width,
        fb.height,
        fb.bpp * 8,
        fb.pitch
    ));

    // Build the terminal and clear the screen. From here the kernel's boot floor stops writing - it
    // releases the framebuffer the first time it successfully delivers console output to us, which is
    // strictly after this point, so there is never a window with two writers or with none.
    let mut term = Term::new(fb);
    let (rows, cols) = term.dims();
    ctx.log_fmt(format_args!("console: terminal {} cols x {} rows", cols, rows));
    ctx.log("console: serving the display");

    let mut rendered: u64 = 0;
    let mut bytes: u64 = 0;
    loop {
        // TAKE THE WHOLE QUEUE, THEN PAINT ONCE.
        //
        // Scrolling repaints the screen from the shadow grid, and this loop used to do that per
        // message. Under load that is the entire cost of the terminal: sixteen queued lines meant
        // sixteen full repaints where one shows the same result, and on hardware the service sat at
        // 100% CPU with its queue jammed at 16/16 while every other service idled at 0/16.
        //
        // The damage was not slowness, it was the queue. A full queue has no room for the shell's echo
        // of a keystroke, so typing produced no cursor and no feedback - it reads as a dead keyboard and
        // is not one - and a full-screen `observe` repainting into the same queue tore, showing half a
        // frame and then another. Draining the backlog first and painting once ends all three, because
        // the queue empties at the speed of the copy into the shadow rather than the speed of the pixels.
        //
        // Blocking `recv` for the first message (idle costs nothing), then `try_recv` for whatever else
        // has already arrived, TO A HARD BOUND OF `DRAIN_BOUND` (§26.6 - a bound that is stated must
        // also be enforced).
        //
        // This comment used to claim the drain was "bounded 16" because only 16 can be queued. That
        // was false, and the kernel guarantees it is false: dequeueing a message takes any parked
        // sender and promotes its pending_send straight into the queue, so while a writer is blocked
        // EVERY try_recv finds another message waiting. The loop ran for as long as the producer kept
        // writing - exactly the case the claim said could not happen.
        //
        // The cost was not subtle. flush() sits after this loop, so a console that never leaves it
        // never repaints: sustained output froze the display. It never blocked either, so it stayed
        // runnable and held its core, and a writer parked on the full queue then waited whole quanta
        // to be rescheduled (measured: 2.8 timer ticks and ~25 ms per park, with the console charged
        // 60% of its core while its own accounting - taken at the loop's tail - recorded exactly
        // zero, because the tail was unreachable).
        //
        // Counting messages instead of trusting the queue to run dry bounds the work per pass, paints
        // at least once every DRAIN_BOUND messages, and returns to a blocking recv so the core goes to
        // whoever is waiting for it. A producer
    // Messages drained per pass before painting and returning to a blocking `recv`. The endpoint
    // queue is 16 deep (§8.5), so one pass can still absorb a full backlog in a single paint - which
    // is the batching this loop exists for - without the drain becoming unbounded when the producer
    // refills as fast as it is emptied.
    const DRAIN_BOUND: usize = 16;

    // Paints that exceeded the report threshold. Owned loop state, not a static (Invariant 9).
    let mut slow_paints: u32 = 0;
    // Cumulative CYCLES spent inside `flush()`, and cumulative cycles spent WORKING (not waiting).
    //
    // RAW CYCLES, converted once at the report - never per sample. The first cut of this divided
    // each sample down to whole milliseconds and ACCUMULATED THE QUOTIENT, so every paint under
    // 1 ms contributed exactly 0 and the total stayed 0 no matter how many there were. It reported
    // "0 ms painting, 0 ms busy" on a machine that visibly crawled, and that reading carried no
    // information at all: it could not tell free apart from just-under-the-unit.
    //
    // Reported as a PERCENTAGE of the elapsed window, which is the honest form here. A ratio of two
    // cycle counts needs no scale factor, so it survives a TSC whose calibration is wrong - and this
    // family of machine has exactly that problem. An absolute millisecond figure would not.
    let mut paint_cycles: u64 = 0;
    let mut busy_cycles: u64 = 0;
    let mut window_start = ctx.read_tsc();
        // that keeps writing cannot hold this loop here.
        // BUSY vs BLOCKED. Painting measured 0 ms while 500 messages still took seconds, so the cost
        // is either upstream of this service or in the terminal state machine - and `flush()` timing
        // cannot tell those apart. The clock starts when `recv` RETURNS, so everything this service
        // does with a message is counted, and everything it spends waiting for one is not.
        //
        // busy near the elapsed time -> the console is the bottleneck, and `put_bytes` is where to
        // look, since the paint is already known to be free. busy near zero -> this service is idle
        // and waiting, the producer or the IPC path is slow, and no console change would help.
        let mut msg = ctx.recv();
        let t_busy0 = ctx.read_tsc();
        let mut drained: usize = 0;
        loop {
            // A request carries a REPLY CAP; console output never does. That, not the payload, is what
            // tells the two apart - a byte stream can contain any bytes at all, so discriminating on
            // content would mean a console write of the wrong single byte silently became a request.
            match ctx.take_pending_cap() {
                // Answered immediately and NOT batched: the caller is blocked on this reply, and a
                // dimensions query is cheap. Deferring it behind a paint would make every client wait
                // for pixels it is not asking about.
                Some(reply_cap) => {
                    term.flush();
                    reply_dims(&ctx, reply_cap, &term, msg.payload_bytes());
                }
                None => {
                    let body = msg.payload_bytes();
                    term.put_bytes(body);
                    rendered += 1;
                    bytes += body.len() as u64;
                    if rendered % RENDER_REPORT == 0 {
                        // Painting time against wall-clock time between these lines is the whole
                        // question: if most of the elapsed second is in here, the paint is the
                        // bottleneck; if little of it is, the cost is upstream (per-message IPC,
                        // scheduling) and making the paint cheaper would buy nothing.
                        // AN INSTRUMENT THAT CANNOT MEASURE MUST SAY SO (invariant 12). `read_tsc`
                        // is a syscall that returns 0 on failure, so a dead clock makes every delta
                        // 0 and prints a confident "0%" that is indistinguishable from a genuinely
                        // idle console. This reported 0% while the kernel's own counters showed the
                        // same core busy, and a silent zero is what made that ambiguous. Report the
                        // raw cycles too, and refuse to dress a dead clock up as a measurement.
                        let now = ctx.read_tsc();
                        let elapsed = now.wrapping_sub(window_start);
                        if now == 0 || elapsed == 0 {
                            ctx.log_fmt(format_args!(
                                "console: rendered {} messages, {} bytes, CLOCK UNAVAILABLE (read_tsc {}) - no timing",
                                rendered, bytes, now
                            ));
                        } else {
                            ctx.log_fmt(format_args!(
                                "console: rendered {} messages, {} bytes, painting {}% busy {}% of elapsed ({} paint, {} busy, {} elapsed cycles)",
                                rendered,
                                bytes,
                                paint_cycles.saturating_mul(100) / elapsed,
                                busy_cycles.saturating_mul(100) / elapsed,
                                paint_cycles,
                                busy_cycles,
                                elapsed
                            ));
                        }
                        paint_cycles = 0;
                        busy_cycles = 0;
                        window_start = ctx.read_tsc();
                    }
                }
            }
            drained += 1;
            if drained >= DRAIN_BOUND {
                break;
            }
            match ctx.try_recv() {
                Some(next) => msg = next,
                None => break,
            }
        }
        // HOW LONG A PAINT ACTUALLY TAKES, because the answer decides the fix and guessing has been
        // expensive. This display is 3840x2160 at 32bpp - 31.6 MB - and the kernel maps the
        // framebuffer NON-CACHEABLE, the way a driver's register BAR is mapped. Uncached stores go to
        // the bus one at a time, so if a full repaint is hundreds of milliseconds the memory TYPE is
        // the whole story and write-combining is the fix; if it is a handful, the cost is elsewhere
        // (per-message IPC, or repainting when little changed) and mapping would not help.
        //
        // Bounded and quiet: only a paint over 20 ms is reported, and only every 32nd after the first
        // few, so a fast display prints nothing at all and a slow one cannot flood the log it is
        // already struggling to render.
        let t0 = ctx.read_tsc();
        term.flush();
        paint_cycles = paint_cycles.saturating_add(ctx.read_tsc().wrapping_sub(t0));
        busy_cycles = busy_cycles.saturating_add(ctx.read_tsc().wrapping_sub(t_busy0));
        // ACCUMULATE, do not threshold. The first cut of this only reported a paint over 20 ms and so
        // printed NOTHING - while 500 messages still took 8.1 seconds to render. A per-paint peak was
        // the wrong question: at ~62 tiny messages a second, paints just UNDER the threshold can still
        // consume the entire second, and a threshold is blind to exactly that. The total is not.
        let _ = slow_paints;
    }
}

/// Answer a request on the caller's reply cap.
///
/// Sent with `try_send`, never `send`: this service is a server, and §8.9 requires the reply direction
/// to be non-blocking or a slow caller can wedge the terminal for everyone. A failure is reported rather
/// than swallowed - the caller is blocked on this reply, and dropping it silently would leave it to time
/// out with no idea why (§26.7).
fn reply_dims(ctx: &ServiceContext, reply_cap: godspeed_sdk::CapHandle, term: &Term, req: &[u8]) {
    if req.first() != Some(&REQ_DIMS) {
        ctx.log("console: request with an unknown opcode - dropping it");
        ctx.remove_cap(reply_cap);
        return;
    }
    let (rows, cols) = term.dims();
    let reply = Message::from_bytes(&[rows as u8, (rows >> 8) as u8, cols as u8, (cols >> 8) as u8]);
    if ctx.try_send_by_handle(reply_cap, &reply).is_err() {
        ctx.log("console: could not reply to a dims request - the caller will see it as unavailable");
    }
}
