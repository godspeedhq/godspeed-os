// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
#![no_main]
//! `control` - the COM2 operator/test channel, as a service rather than kernel code.
//!
//! **Why this exists.** §4.4 forbids "developer tooling" in the kernel by name, and a 123-line command
//! interpreter (`kernel/src/control.rs`) sat inside it anyway, parsing lines off a serial port and
//! killing services (finding C1-6). The kernel keeps the byte read - a UART is hardware, and §11.4
//! already sanctions the kernel owning a serial console - and this service owns what the bytes MEAN.
//!
//! **Why it is not special.** An earlier attempt amended the constitution to keep this in the kernel,
//! arguing that Chaos would kill a control service mid-test. That argument was wrong and was reverted:
//! a control service is in the supervisor's watched set, so Chaos killing it means it comes BACK, and
//! the recovery chain still ends at the kernel. The channel does not need protection, it needs fault
//! tolerance - which is the discipline every other client already owes (§14.3). Only impossibility
//! earns kernel residency, and nothing here is impossible outside it.
//!
//! **Its authority is named, not implied.** SERVICE_CONTROL to kill, SPAWN to restart, FIRE_IRQ to
//! inject a test interrupt, INTROSPECT to read the byte. In the kernel those were all implicit - ring 0
//! holds every authority - so writing them down is a reduction even though the syscall and capability
//! pins grew by one each to make it possible.

use godspeed_sdk::{ServiceContext, ipc::Message};

/// Kernel introspection query that pops one byte from COM2, or -1 when the port is empty.
const Q_COM2_BYTE: u64 = 21;

const LINE_MAX: usize = 128;

/// Parse and execute one command line. Unknown input is REPORTED, never ignored: an operator typing a
/// typo into a serial port should be told, not left wondering whether the machine is wedged.
fn execute(ctx: &ServiceContext, line: &str) {
    let mut parts = line.split_ascii_whitespace();
    match parts.next() {
        Some("KILL") => match parts.next() {
            Some(name) => {
                ctx.log_fmt(format_args!("control: KILL {}", name));
                match ctx.kill(name) {
                    Ok(()) => ctx.log_fmt(format_args!("control: {} killed", name)),
                    Err(_) => ctx.log_fmt(format_args!("control: {} not found", name)),
                }
            }
            None => ctx.log("control: KILL missing name"),
        },
        Some("RESTART") => match parts.next() {
            Some(name) => {
                let core: Option<u32> = parts.next().and_then(|s| s.parse().ok());
                ctx.log_fmt(format_args!("control: RESTART {} core={:?}", name, core));
                // ASK THE SUPERVISOR. This used to kill and then spawn here, which meant asking the
                // KERNEL to spawn by name - and that only ever worked because the kernel held every
                // service image. Restart authority is the supervisor's (14.4), and once an image
                // lives there it is the only thing that CAN respawn it
                // (`docs/service-ownership.md`).
                //
                // Bounded (26.6): a deadline, not an open wait. A supervisor that is mid-respawn of
                // itself must never hang the operator channel - the harness would read a hang as a
                // dead machine.
                let mut buf = [0u8; 64];
                buf[0] = CMD_MARKER;
                buf[1] = CMD_RESTART;
                buf[2..6].copy_from_slice(&core.unwrap_or(u32::MAX).to_le_bytes());
                let nb = name.as_bytes();
                let n  = nb.len().min(buf.len() - 6);
                buf[6..6 + n].copy_from_slice(&nb[..n]);

                match ctx.request_with_reply_call_err(
                        "supervisor", &Message::from_bytes(&buf[..6 + n]), 10) {
                    Ok(Some(reply)) => match reply.payload_bytes().first() {
                        Some(&CMD_OK) => ctx.log_fmt(format_args!("control: {} restarted", name)),
                        Some(&CMD_UNKNOWN) =>
                            ctx.log_fmt(format_args!("control: restart failed: supervisor did not understand the request for {}", name)),
                        _ => ctx.log_fmt(format_args!("control: restart failed: supervisor could not restart {}", name)),
                    },
                    // LOUD on both (invariant 12). A silent timeout here would read as a restart that
                    // worked, which is the one thing an operator channel must never imply.
                    Ok(None) => ctx.log_fmt(format_args!(
                        "control: restart failed: supervisor did not answer within 10s ({})", name)),
                    Err(e)   => ctx.log_fmt(format_args!(
                        "control: restart failed: supervisor unreachable ({:?}) - {}", e, name)),
                }
            }
            None => ctx.log("control: RESTART missing name"),
        },
        Some("FIRE_IRQ") => match parts.next().and_then(|s| s.parse::<u8>().ok()) {
            Some(irq) => {
                ctx.log_fmt(format_args!("control: FIRE_IRQ {}", irq));
                if !ctx.fire_irq(irq) {
                    ctx.log("control: FIRE_IRQ refused - FIRE_IRQ capability not held");
                }
            }
            None => ctx.log("control: FIRE_IRQ missing irq"),
        },
        Some(other) => ctx.log_fmt(format_args!("control: unknown command '{}'", other)),
        None => {}
    }
}

/// The supervisor's command wire format. Kept in step with `services/supervisor/src/main.rs`; a
/// mismatch shows up immediately as CMD_UNKNOWN rather than silently doing the wrong thing.
const CMD_MARKER:  u8 = 0x01;
const CMD_RESTART: u8 = b'R';
const CMD_OK:      u8 = 0;
const CMD_UNKNOWN: u8 = 2;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("control: serving the COM2 operator channel (C1-6: out of the kernel)");
    let mut buf = [0u8; LINE_MAX];
    let mut len = 0usize;

    // Idle poll period, in milliseconds: fast while an operator is typing, backing off while the port
    // is silent. See the backoff comment in the loop for why a FIXED period was wrong twice over.
    const IDLE_MIN_MS: u64 = 10;
    const IDLE_MAX_MS: u64 = 250;
    const IDLE_GROWTH: u64 = 3;
    let mut idle_ms: u64 = IDLE_MIN_MS;

    loop {
        // Drain what is waiting, then sleep. BOUNDED per pass so a stuck port cannot monopolise this
        // task, and the bound is a count because it bounds WORK PER PASS, not a duration - the loop
        // returns to the scheduler either way (Commandment VIII is about counts standing in for time).
        let mut budget = 256u32;
        let mut got_any = false;
        while budget > 0 {
            budget -= 1;
            let b = match ctx.com2_byte() {
                Some(b) => b,
                None => break, // port empty
            };
            got_any = true;
            if b == b'\n' || b == b'\r' {
                if len > 0 {
                    if let Ok(line) = core::str::from_utf8(&buf[..len]) {
                        execute(&ctx, line);
                    }
                    len = 0;
                }
            } else if len < LINE_MAX - 1 {
                buf[len] = b;
                len += 1;
            }
        }
        if got_any {
            // Something arrived: an operator is talking to us, so answer at full speed.
            idle_ms = IDLE_MIN_MS;
        } else {
            // Nothing waiting: sleep, and BACK OFF the longer nothing comes. Time here only conserves
            // CPU, it does not decide correctness - the truth is still "a byte arrived"
            // (Commandment VIII), and the backoff changes only how often we ask.
            //
            // A fixed 10 ms poll had two problems. It never stops: on a machine with no COM2 at all -
            // and this port is absent on plenty of them, the kernel says so at boot and suppresses the
            // reads - this task woke a hundred times a second, forever, to ask a port that does not
            // exist whether it had anything to say. And 10 ms is exactly the scheduler quantum, so the
            // wakeups resonated with the tick that samples per-task CPU: whoever is running when the
            // tick fires is charged the whole 10 ms, and a task waking on precisely that period gets
            // charged far more than it uses. It measured 36% of a core for work that is a few
            // microseconds of port reads.
            //
            // Backing off to IDLE_MAX_MS costs latency only on the FIRST byte after a quiet spell,
            // after which the reset above restores full speed for the rest of the line. The growth
            // factor keeps the period off any exact multiple of the quantum, so it cannot settle into
            // that resonance again.
            ctx.sleep(ctx.duration_cycles(idle_ms));
            idle_ms = (idle_ms * IDLE_GROWTH).min(IDLE_MAX_MS);
        }
    }
}
