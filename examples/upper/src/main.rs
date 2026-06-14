//! `upper` — a pipe filter: receives text messages and emits them uppercased.
//!
//! The consumer side of a capability-mediated pipe (`greet | upper`). It recvs
//! on its own endpoint — the endpoint the shell grants the *producer* a SEND
//! cap to. `upper` needs no knowledge of who is sending; it just transforms
//! whatever arrives. That is the point: composition without coupling.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Announce ourselves to the registry so a built-in pipe producer (the shell capturing
    // `echo … | upper`) can resolve our endpoint at runtime and send us its text. A sink
    // service still needs no knowledge of *who* sends — only that it is discoverable.
    let _ = ctx.register("upper");
    ctx.log("upper: ready");

    let mut out = [0u8; 256];
    loop {
        let msg = ctx.recv();
        let src = msg.payload_bytes();
        let n = src.len().min(out.len());
        for i in 0..n {
            let c = src[i];
            // ASCII lowercase → uppercase; everything else passes through.
            out[i] = if c.is_ascii_lowercase() { c - 32 } else { c };
        }
        ctx.log_fmt(format_args!(
            "upper: {}",
            core::str::from_utf8(&out[..n]).unwrap_or("<invalid utf8>")
        ));
    }
}
