//! `ping` — sends a message to `pong` on every scheduler tick.
//!
//! Pinned to Core 0 (§23.1). On `EndpointDead`, reacquires a fresh SEND cap
//! via the kernel name registry and resumes (§14.2, test 10).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, IpcError};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("ping: starting");

    let mut counter: u64 = 0;
    let mut success_count: u64 = 0;

    loop {
        counter += 1;
        let payload = make_payload(counter);
        let msg = Message::from_bytes(&payload[..payload_len(&payload)]);

        match ctx.try_send("pong", &msg) {
            Ok(()) => {
                success_count += 1;
                if success_count == 20 {
                    ctx.log("ping: sent 20 messages");
                }
            }
            Err(IpcError::EndpointDead) => {
                ctx.log("ping: pong endpoint dead, reacquiring via kernel registry");
                match ctx.reacquire_cap("pong") {
                    Ok(_) => ctx.log("ping: pong cap reacquired, resuming"),
                    Err(_) => ctx.log("ping: reacquire failed, retrying next tick"),
                }
            }
            Err(IpcError::QueueFull) => {
                // pong is alive but busy; yield and retry.
            }
            Err(_) => {}
        }

        ctx.yield_cpu();
    }
}

/// Format the counter as ASCII decimal into a fixed buffer.
fn make_payload(n: u64) -> [u8; 20] {
    let mut buf = [0u8; 20];
    let mut tmp = [0u8; 20];
    let mut i = 0;
    let mut v = if n == 0 { 1 } else { n };
    if n == 0 { tmp[0] = b'0'; i = 1; }
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    // Reverse into buf.
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    buf
}

fn payload_len(buf: &[u8; 20]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(20)
}
