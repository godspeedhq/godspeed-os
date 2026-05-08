//! `ping` — demonstration service. Sends a message to `pong` every second.
//!
//! Pinned to Core 0 (§23.1). Acceptance criteria §23.2:
//!   - `osdev logs ping` shows a send every second.
//!   - After `osdev restart pong`, ping observes EndpointDead, looks up via
//!     registry, and continues with the fresh cap (§6.B test).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, IpcError};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("ping: starting");

    let mut pong_cap = ctx.capability("ipc_send.pong")
        .expect("ping: missing ipc_send.pong cap");

    loop {
        let msg = Message::from_bytes(b"ping");

        match ctx.try_send("pong", &msg) {
            Ok(()) => {
                ctx.log("ping: sent");
            }
            Err(IpcError::EndpointDead) => {
                ctx.log("ping: pong endpoint dead, reacquiring via registry");
                // Re-lookup via registry; fresh cap may point to a new core (§14.2).
                pong_cap = reacquire_pong(&ctx);
            }
            Err(IpcError::QueueFull) => {
                // pong is alive but busy; retry next tick.
                ctx.log("ping: queue full, retrying");
            }
            Err(e) => {
                ctx.log_fmt(format_args!("ping: unexpected error: {:?}", e));
            }
        }

        sleep_one_second(&ctx);
    }
}

fn reacquire_pong(ctx: &ServiceContext) -> godspeed_sdk::CapHandle {
    todo!("send lookup(\"pong\") to registry; return fresh cap")
}

fn sleep_one_second(ctx: &ServiceContext) {
    todo!("yield in a loop until ~1 s has elapsed (timer or spin-wait)")
}
