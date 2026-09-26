// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
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

use godspeed::{self as gs, ipc::Message, ServiceContext};

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("cap-grant: starting");

    // 1. A grantable cap we already hold: our own SEND|GRANT cap to our endpoint,
    //    minted from the contract at spawn. This is the cap a service hands out so
    //    others may call it back (Commandment VII - authority is an explicit cap).
    let self_cap = match gs::cap::self_grant(&ctx) {
        Ok(c) => c,
        Err(_) => { ctx.log("cap-grant: no grantable endpoint cap to give"); gs::ipc::park(&ctx) }
    };

    // 2. Duplicate the cap to give away, keeping the original so we can re-grant after a
    //    peer restart (Commandment IX - plan for recovery). The copy carries the SAME rights;
    //    rights narrow where a cap is minted or granted, never on the copy (§7.3).
    let gift = match gs::cap::duplicate(&ctx, self_cap) {
        Ok(c) => c,
        Err(_) => { ctx.log("cap-grant: could not duplicate the cap to give away"); gs::ipc::park(&ctx) }
    };

    // 3. Transfer the gift to "receiver" inside an IPC message. The kernel verifies
    //    the cap carries GRANT (§7.4); on success it MOVES to the receiver and is
    //    removed from our table - authority transfers, it does not duplicate
    //    (§7.6, §8.5). A cap without GRANT is refused with CapNotGrantable and kept.
    let note = Message::from_bytes(b"a cap to call me back");
    match gs::cap::acquire(&ctx, "receiver") {
        Ok(receiver) => match gs::ipc::send_granting(&ctx, receiver, gift, &note) {
            Ok(()) => ctx.log("cap-grant: granted a cap to receiver (we no longer hold the copy)"),
            // The capability lacked GRANT. Nothing moved, so the gift is STILL OURS and the slot is
            // ours to reclaim - which is why this arm is distinct from "the peer was not there".
            Err(gs::Error::PermissionDenied) =>
                ctx.log("cap-grant: refused - the cap lacks GRANT; authority cannot be widened"),
            Err(_) => ctx.log("cap-grant: receiver unavailable; a real client would retry"),
        },
        Err(_) => ctx.log("cap-grant: 'receiver' not registered (expected when run standalone)"),
    }

    // The RECEIVER side, in its own service, completes the transfer:
    //
    //     let _carrier = gs::ipc::recv(&ctx);                 // the message that carried the cap
    //     if let Some(granted) = gs::ipc::take_sent_cap(&ctx) {
    //         // `granted` is now in OUR table - use it to call the granter back.
    //         let _ = gs::ipc::send_to(&ctx, granted, &Message::from_bytes(b"thanks"));
    //     }

    gs::ipc::park(&ctx)
}
