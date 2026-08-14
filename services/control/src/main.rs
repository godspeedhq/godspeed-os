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

use godspeed_sdk::ServiceContext;

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
                // Kill first, then spawn. A kill that finds nothing is not an error here: RESTART of a
                // service that already died is exactly the case the harness uses.
                let _ = ctx.kill(name);
                let spawned = match core {
                    Some(c) => ctx.spawn_on(name, c),
                    None => ctx.spawn(name),
                };
                match spawned {
                    Ok(()) => ctx.log_fmt(format_args!("control: {} restarted", name)),
                    Err(e) => ctx.log_fmt(format_args!("control: restart failed: {:?}", e)),
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

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("control: serving the COM2 operator channel (C1-6: out of the kernel)");
    let mut buf = [0u8; LINE_MAX];
    let mut len = 0usize;

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
        if !got_any {
            // Nothing waiting: yield the core rather than spin. Time here only conserves CPU, it does
            // not決 decide correctness - the truth is still "a byte arrived" (Commandment VIII).
            ctx.sleep(ctx.duration_cycles(10));
        }
    }
}
