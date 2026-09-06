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

///
/// Present because the first hardware boot left a question plain observation could not settle: the
/// terminal's queue sat full at 16/16 while it reported ~0% CPU, which is the signature of BOTH "too
/// slow to keep up" and "not running at all", and those need opposite fixes. A count that climbs says
/// the first; a count that stops says the second (§26.7 - measure, do not guess).

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column, and
    // worse, every METRIC published lands under a BLANK owner: the metric key is (owner, name), so
    // ten unnamed services all collide into one row and their counters interleave. Observed as a
    // single `msgs.received 1920` belonging to nobody.
    ctx.trace_as("console");
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


    // Passes completed, for context in the diagnostics below.
    let mut passes: u64 = 0;
    // PAINT CADENCE. A pass drains for at most this long before painting and returning to a blocking
    // `recv`. 16 ms is about one frame; a display that repaints that often reads as smooth.
    //
    // A DURATION, not a count, and the difference is the whole fix. Painting is the dominant cost
    // here - a scroll repaints the screen, measured at roughly two thirds of this service's work -
    // so the only way bulk output is ever cheap is to collapse many scrolls into ONE repaint. That
    // is what draining the backlog buys. Bounding the drain by a message COUNT destroyed exactly
    // that: a full repaint every 16 lines, which on hardware was five times slower overall and
    // rendered line-at-a-time. Leaving it unbounded was the opposite failure - it batched perfectly
    // and never reached the paint at all, so sustained output froze the screen.
    //
    // A deadline gets both: batch everything that has arrived, however much that is, but never defer
    // the repaint past a frame. Bounded (§26.6) by the clock rather than by a tally, because a count
    // is not a duration - it means a different amount of time on every machine.
    const PAINT_DEADLINE_MS: u64 = 16;
    // Ceiling for the adaptive deadline below: even a very slow display must repaint this often.
    const PAINT_DEADLINE_MAX_MS: u64 = 100;
    // Fallback for an uncalibrated clock: `tsc_ticks_per_10ms` reports 0, and a deadline derived
    // from it would be meaningless. Bound by count then, and say so rather than silently painting
    // per line.
    const DRAIN_BOUND: usize = 16;
    let per_10ms = ctx.tsc_ticks_per_10ms();
    // Cycles per microsecond, for reporting a paint in units a human can compare against a frame.
    let per_us = (per_10ms / 10_000).max(1);
    let mut paint_deadline: u64 = if per_10ms == 0 {
        ctx.log("console: TSC uncalibrated - paint cadence bounded by message count, not time");
        0
    } else {
        per_10ms.saturating_mul(PAINT_DEADLINE_MS) / 10
    };
    // How slow a single repaint must be before it is worth reporting, and how many times to say
    // so. See the regression-guard comment at the report itself.
    const SLOW_PAINT_FLOOR_US: u64 = 250_000;
    const SLOW_PAINT_REPORTS: u32 = 3;
    let mut max_paint_us: u64 = 0;
    let mut slow_paint_reports: u32 = 0;
    // Passes that ended with the escape parser mid-sequence. See the watch in the loop.
    let mut stranded: u64 = 0;
    // ADAPTIVE PAINT CADENCE, derived from what a paint actually costs on THIS display.
    //
    // A full repaint costs the same whether it shows one new line or sixteen, so batching longer is
    // strictly better for throughput and the only price is update latency. That makes a fixed
    // deadline wrong in both directions: 16 ms asks 60 fps of a 4K display that cannot repaint
    // faster than about 40, so it is always late and spends most of its life on pixels, while on a
    // fast display a larger fixed value would batch more than it needs to.
    //
    // Measured on the slow display: a paint costs 20-25 ms against a 16 ms deadline, and the service
    // reported 63% of its time painting - against P/(D+P) = 25/(16+25) = 61% predicted. Aiming the
    // deadline at twice the paint cost holds that share near a third, whatever the machine.
    //
    // Smoothed, so one unusually expensive repaint cannot lurch the cadence, and clamped at both
    // ends: never below the 16 ms floor (a fast display stays smooth), never above
    // PAINT_DEADLINE_MAX_MS (a pathological one still refreshes).
    let deadline_floor = paint_deadline;
    let deadline_ceil = if per_10ms == 0 { 0 } else {
        per_10ms.saturating_mul(PAINT_DEADLINE_MAX_MS) / 10
    };
    let mut paint_ewma: u64 = 0;
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
        // Pass start, for the paint deadline below. Not diagnostics: this is what bounds the drain.
        let t_pass0 = ctx.read_tsc();
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
                }
            }
            drained += 1;
            // Deadline first; the count is only the uncalibrated-clock fallback.
            if paint_deadline != 0 {
                if ctx.read_tsc().wrapping_sub(t_pass0) >= paint_deadline {
                    break;
                }
            } else if drained >= DRAIN_BOUND {
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
        // STRANDED PARSER WATCH. A writer killed between an ESC and the sequence's final byte
        // leaves the escape state machine mid-sequence, and every byte after that is swallowed as
        // sequence body instead of being rendered. Chaos kills writers by design, so this is the
        // shape to look for behind "the console started rendering funny, and respawning it fixed
        // it": the damage lives in THIS service's parser state, not on the screen.
        //
        // Reported the first few times and then every 64th, so a genuinely stuck parser is loud
        // without flooding the log through the very console that is misbehaving. A sequence
        // legitimately spanning a message boundary shows up here once and then clears; a stranded
        // one never clears.
        let esc = term.esc_state();
        if esc != 0 {
            stranded += 1;
            if stranded <= 5 || stranded % 64 == 0 {
                ctx.log_fmt(format_args!(
                    "console: escape parser mid-sequence (state {}) at end of pass {} - {} passes so far",
                    esc, passes, stranded
                ));
            }
        }
        let t0 = ctx.read_tsc();
        term.flush();
        let this_paint_cycles = ctx.read_tsc().wrapping_sub(t0);
        if per_10ms != 0 {
            // EWMA over the last few paints, then aim at twice it.
            paint_ewma = if paint_ewma == 0 {
                this_paint_cycles
            } else {
                (paint_ewma.saturating_mul(3).saturating_add(this_paint_cycles)) / 4
            };
            paint_deadline = paint_ewma
                .saturating_mul(2)
                .clamp(deadline_floor, deadline_ceil);
        }
        passes += 1;
        if per_10ms != 0 {
            let this_paint = this_paint_cycles / per_us;
            if this_paint > max_paint_us {
                max_paint_us = this_paint;
                // REGRESSION GUARD, not a measurement. A repaint on this display costs tens of
                // milliseconds; it cost 596 ms when the framebuffer was mapped strong-uncacheable,
                // because every pixel write went to the bus alone. The floor sits well above healthy
                // and well below that, so this is SILENT on a working machine and loud the moment
                // the mapping regresses - which is a one-word change in the kernel and otherwise
                // shows up only as "the display feels slow" months later.
                if this_paint >= SLOW_PAINT_FLOOR_US && slow_paint_reports < SLOW_PAINT_REPORTS {
                    slow_paint_reports += 1;
                    ctx.log_fmt(format_args!(
                        "console: paint took {} us - far slower than this display should be;                          check the framebuffer memory type (pass {})",
                        this_paint, passes
                    ));
                }
            }
        }
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
