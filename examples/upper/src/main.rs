// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `upper` - a pipe FILTER: receives text and re-emits it uppercased.
//!
//! A filter stage of a capability-mediated pipe. The shell sends it the previous stage's
//! output on `upper`'s endpoint, and `upper` sends the transformed text back to the shell
//! over the SEND cap the shell delegated at spawn (`send_peers[0]`). So `upper` can sit
//! anywhere in a chain - `echo hi | upper | write /f` - not just at the end. It needs no
//! knowledge of *who* sends or *where* its output goes; the shell brokers both. That is the
//! point: composition without coupling.
//!
//! Protocol: for each input message, emit the uppercased bytes; on a lone EOT (0x04) marker,
//! forward EOT (end of this stream) and wait for the next. The shell drains until that EOT.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, ServiceContext};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Path C (Phase 4): no self-registration - the kernel name-directory records "upper" at spawn,
    // so the shell resolves our endpoint by name via the directory (`acquire_send_grant_cap`).
    ctx.log("upper: ready");

    let mut out = [0u8; 4096]; // one message's worth (MAX_PAYLOAD)
    loop {
        let msg = ctx.recv();
        let src = msg.payload_bytes();
        // send_peers[0] is the SEND cap to the shell (our downstream), delegated at spawn.
        let down = match ctx.send_peer_at(0) { Some(p) => p, None => continue };
        if src == [0x04] {
            // End of this input stream - forward the EOT so the shell stops draining.
            let _ = ctx.send_by_handle(down, &Message::from_bytes(&[0x04]));
            continue;
        }
        let n = src.len().min(out.len());
        for i in 0..n {
            let c = src[i];
            // ASCII lowercase → uppercase; everything else passes through.
            out[i] = if c.is_ascii_lowercase() { c - 32 } else { c };
        }
        let _ = ctx.send_by_handle(down, &Message::from_bytes(&out[..n]));
    }
}
