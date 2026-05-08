//! `logger` — structured log sink (§11.4).
//!
//! On startup: drain the kernel ring buffer before accepting new messages.
//! After draining: receive log messages from any service holding `log_write`
//! and write them to the serial console (and later to disk if block driver is up).
//!
//! logger is restartable. Log history before the restart is lost; that is
//! acceptable — the kernel ring buffer preserves the most recent 16 KiB.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Drain the kernel ring buffer accumulated before logger started (§11.4).
    ctx.drain_kernel_ring_buffer();

    ctx.log("logger: ready");

    loop {
        let msg = ctx.recv_log_message();
        emit(&ctx, &msg);
    }
}

fn emit(_ctx: &ServiceContext, _msg: &Message) {
    todo!("format [service_name] level: text and write to serial")
}
