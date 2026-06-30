// SPDX-License-Identifier: Apache-2.0
//! cap-grant - transfer a capability to another service over IPC (§7.4, §8.5).
//!
//! Authority on Godspeed is a capability: an unforgeable token you HOLD. To let
//! another service act, you do not flip a flag or share memory - you GRANT it a
//! capability. This service shows the granter side end to end: hold a grantable
//! cap, derive a copy to give away, and transfer it inside a message. The kernel
//! enforces the GRANT right and removes the cap from our table on success, so
//! authority MOVES rather than silently duplicating (§7.6).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, IpcError, CapError};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("cap-grant: starting");

    // 1. A grantable cap we already hold: our own SEND|GRANT cap to our endpoint,
    //    minted from the contract at spawn. This is the cap a service hands out so
    //    others may call it back (Commandment VII - authority is an explicit cap).
    let self_cap = match ctx.self_grant_handle() {
        Some(c) => c,
        None => { ctx.log("cap-grant: no grantable endpoint cap to give"); ctx.park() }
    };

    // 2. Derive a COPY to give away, keeping the original so we can re-grant after a
    //    peer restart (Commandment IX - plan for recovery). A derived cap can only
    //    narrow rights, never widen them (§7.3, non-escalating).
    let gift = match ctx.derive_cap(self_cap) {
        Some(c) => c,
        None => { ctx.log("cap-grant: derive_cap failed"); ctx.park() }
    };

    // 3. Transfer the gift to "receiver" inside an IPC message. The kernel verifies
    //    the cap carries GRANT (§7.4); on success it MOVES to the receiver and is
    //    removed from our table - authority transfers, it does not duplicate
    //    (§7.6, §8.5). A cap without GRANT is refused with CapNotGrantable and kept.
    let note = Message::from_bytes(b"a cap to call me back");
    match ctx.acquire_send_cap("receiver") {
        Some(receiver) => match ctx.send_with_cap_by_handle(receiver, gift, &note) {
            Ok(()) => ctx.log("cap-grant: granted a cap to receiver (we no longer hold the copy)"),
            Err(IpcError::CapError(CapError::CapNotGrantable)) =>
                ctx.log("cap-grant: refused - the cap lacks GRANT; authority cannot be widened"),
            Err(_) => ctx.log("cap-grant: receiver unavailable; a real client would retry"),
        },
        None => ctx.log("cap-grant: 'receiver' not registered (expected when run standalone)"),
    }

    // The RECEIVER side, in its own service, completes the transfer:
    //
    //     let _carrier = ctx.recv();                 // the message that carried the cap
    //     if let Some(granted) = ctx.take_pending_cap() {
    //         // `granted` is now in OUR table - use it to call the granter back.
    //         let _ = ctx.send_by_handle(granted, &Message::from_bytes(b"thanks"));
    //     }

    ctx.park()
}
