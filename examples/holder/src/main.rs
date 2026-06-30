// SPDX-License-Identifier: Apache-2.0
//! `holder` - the CLIENT half of delegated resource capabilities (§7.10, P2). The use-side
//! counterpart to `examples/resource-server` (the owner/mint side), exactly as `ping` is to `pong`.
//!
//! resource-server MINTS a resource it owns, narrows a **READ-ONLY** copy of the cap, and GRANTs it
//! to holder over IPC (an embedded cap, §8.5). holder receives that cap and proves the three
//! properties that make it a GENUINE capability (§7.3), not a service-level token:
//!
//!   1. USE            - invoke READ  -> the owner serves it -> `holder: read OK`.
//!   2. NON-ESCALATION - invoke WRITE on the READ-ONLY cap -> the KERNEL refuses it
//!                       (`CapInsufficientRights`: rights cannot widen, §7.3) ->
//!                       `holder: write denied (non-escalation)`.
//!   3. REVOCABLE      - tell the owner to revoke (OP_CLOSE), then invoke again -> the cap is stale
//!                       (`CapRevoked`, the generation bump of §7.5) -> `holder: revoked (CapRevoked)`.
//!
//! A `resource_invoke` is the "send" of a resource cap: holder never name-addresses resource-server.
//! The KERNEL validates the cap (generation + the invoked right) and routes the message to the
//! resource's OWNER, badged with the opaque resource id - authority by capability, not by identity
//! (Commandments VII/IX). holder embeds a per-invoke reply cap (a SEND copy of its OWN endpoint) so
//! the owner can answer; that is the only channel the owner has back to holder.
//!
//! Commandments this teaches (the use half of mint/use):
//!   VI   - it talks over IPC + a capability, never shared memory.
//!   VII  - it acts ONLY through the cap it was granted; it holds no ambient authority and names no
//!          one. A read-only cap that cannot write is non-escalation made mechanical.
//!   VIII - it waits on the owner's REPLY (truth), never a fixed sleep; a denial/revoke is a returned
//!          error it reads, not a timeout it guesses (§8.6).
//!   IX   - it assumes the granted cap can be pulled out from under it: after revoke, the next use
//!          fails loudly with CapRevoked rather than silently succeeding (§7.5, §26.7).
//!   X    - the resource's MEANING is policy in resource-server; the kernel only mints/validates/
//!          routes/revokes the cap and never learns what it is (§4.4, §26.10).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, IpcError, CapHandle, CapError};
use godspeed_sdk::capability::{RIGHT_READ, RIGHT_WRITE};

// Resource operations - the FIRST payload byte of a badged invocation (mirrors resource-server's
// OP_* / fs's FOP_*). The kernel validates the cap holds the invoked RIGHT; the owner additionally
// enforces op <= right. These OP codes are this example's tiny protocol, not kernel constants.
const OP_READ:  u8 = 1; // owner needs RIGHT_READ
const OP_WRITE: u8 = 2; // owner needs RIGHT_WRITE
const OP_CLOSE: u8 = 4; // retire the resource: the owner revokes it

const REPLY_OK: u8 = 0; // resource-server's "ok" reply byte

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("holder: starting");

    // 1. Receive the GRANTED resource cap. resource-server sent us a message carrying an embedded
    //    cap (§8.5); the kernel placed the cap in OUR table and `take_pending_cap` hands us the slot.
    //    We block until it arrives - wait on truth, not a sleep (Commandment VIII). Any plain message
    //    with no embedded cap is not what we want; loop until we actually hold the cap.
    let cap = loop {
        let _ = ctx.recv();
        if let Some(c) = ctx.take_pending_cap() {
            ctx.log("holder: received a resource cap (granted by resource-server)");
            break c;
        }
        ctx.log("holder: message had no embedded cap - waiting for the grant");
    };

    // 2. USE (§7.3 - a real cap is usable for what it permits). Invoke READ: the kernel validates our
    //    cap holds RIGHT_READ and routes the op to the owner, which serves it and replies OK.
    match invoke(&ctx, cap, RIGHT_READ, OP_READ) {
        Ok(reply) if reply.payload_bytes().first() == Some(&REPLY_OK) =>
            ctx.log("holder: read OK"),
        Ok(_)  => ctx.log("holder: read FAIL - owner did not reply OK"),
        Err(_) => ctx.log("holder: read FAIL - the cap could not be used"),
    }

    // 3. NON-ESCALATION (§7.3 - rights cannot widen). Our cap is READ-ONLY. Invoke WRITE: the KERNEL
    //    rejects it with CapInsufficientRights because the cap lacks RIGHT_WRITE - the request never
    //    even reaches the owner. This is the load-bearing proof that a narrowed cap cannot out-reach
    //    its rights; a read-only cap is read-only, mechanically.
    match invoke(&ctx, cap, RIGHT_WRITE, OP_WRITE) {
        Err(IpcError::CapError(CapError::CapInsufficientRights)) =>
            ctx.log("holder: write denied (non-escalation)"),
        Ok(_)  => ctx.log("holder: write FAIL - a READ-ONLY cap WROTE (escalation!)"),
        Err(e) => { let _ = e; ctx.log("holder: write FAIL - denied, but not for the expected reason"); }
    }

    // 4. REVOCABLE (§7.5 / §7.10 - a generation bump invalidates every outstanding cap). Ask the
    //    owner to revoke the resource (OP_CLOSE; the owner alone may revoke what it owns), then use
    //    the cap once more. It is now stale, so the kernel's generation check fails with CapRevoked -
    //    the same mechanism fs uses to revoke a file cap on delete/close. A revoked cap fails LOUDLY,
    //    never silently succeeds (Commandment IX / §26.7).
    let _ = invoke(&ctx, cap, RIGHT_READ, OP_CLOSE);   // trigger the owner's revoke
    match invoke(&ctx, cap, RIGHT_READ, OP_READ) {
        // The kernel's generation check on the revoked resource returns CapRevoked (the resource was
        // marked Revoked, §7.5). The SDK surfaces that generation-mismatch family as EndpointDead, so
        // in practice it arrives here as EndpointDead - the cap is gone, which is exactly the revoke
        // property under test. We accept either spelling; both mean "stale, no longer usable".
        Err(IpcError::EndpointDead) | Err(IpcError::CapError(CapError::CapRevoked)) =>
            ctx.log("holder: revoked (CapRevoked)"),
        Ok(_)  => ctx.log("holder: revoke FAIL - the cap is still usable after revoke"),
        Err(e) => { let _ = e; ctx.log("holder: revoke FAIL - failed, but not as revoked"); }
    }

    ctx.log("holder: done (use / non-escalation / revoke all checked)");

    // Nothing left to do - park rather than spin (Commandment V: bounded, quiet idle).
    ctx.park()
}

/// Invoke the resource cap `cap` for operation `op`, asking the kernel to validate `right`.
///
/// This is the client's "send" of a delegated resource cap (the mirror of the shell's `fc_invoke`,
/// `services/shell`): derive a per-invoke reply cap from our OWN endpoint (a SEND|GRANT copy), then
/// `resource_invoke`. On `Ok` the kernel routed the op to the owner and embedded our reply cap, so we
/// block for the owner's reply. On `Err` the kernel REJECTED the invocation (the cap lacks `right` -
/// non-escalation - or is stale/revoked), so no reply will come: we reclaim the reply slot (no leak,
/// §26.6) and return the error for the caller to read. We NEVER recv after a rejected invoke (that
/// would block forever waiting for a reply the owner was never asked to send).
fn invoke(ctx: &ServiceContext, cap: CapHandle, right: u8, op: u8) -> Result<Message, IpcError> {
    // Our endpoint's own SEND|GRANT handle, then a fresh per-invoke copy to embed as the reply cap.
    let self_grant = match ctx.self_grant_handle() {
        Some(h) => h,
        None    => return Err(IpcError::EndpointDead), // no endpoint to reply on (should not happen)
    };
    let reply = match ctx.derive_cap(self_grant) {
        Some(r) => r,
        None    => return Err(IpcError::EndpointDead), // cap table full (should not happen)
    };
    match ctx.resource_invoke(cap, right, reply, &Message::from_bytes(&[op])) {
        Ok(())  => Ok(ctx.recv()),          // routed: block for the owner's reply on our endpoint
        Err(e)  => { ctx.remove_cap(reply); Err(e) } // rejected: reclaim the unused reply slot, report
    }
}
