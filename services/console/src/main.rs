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
//! Ordinary IPC, deliberately. `ConsoleWrite` (syscall 23) still writes serial synchronously - serial
//! remains the source of truth and a captured log is unchanged - and then `try_send`s the same bytes to
//! this service's endpoint. So there is no new syscall, no new capability, no polling loop and no second
//! ring buffer in the kernel: the transport is the bounded queue and the blocking `recv` that already
//! exist, and this service simply blocks on its endpoint like every other one.
//!
//! The kernel `try_send`s rather than `send`s, which is the important half: **the kernel never blocks on
//! this service and never depends on it.** If this service is dead, slow, or its queue is full, console
//! bytes are dropped from the *display* and serial still has all of them. That is the same trade the ARM
//! serial mirror already made ("serial is the source of truth; dropping a contended write costs nothing
//! visible"), and it is why the display can be a service at all.
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

    loop {
        let msg = ctx.recv();
        // A request carries a REPLY CAP; console output never does. That, not the payload, is what
        // tells the two apart - a byte stream can contain any bytes at all, so discriminating on
        // content would mean a console write of the wrong single byte silently became a request.
        match ctx.take_pending_cap() {
            Some(reply_cap) => reply_dims(&ctx, reply_cap, &term, msg.payload_bytes()),
            None => term.put_bytes(msg.payload_bytes()),
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
