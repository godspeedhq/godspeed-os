//! resource-server - MINT a delegated resource capability a service OWNS (§7.10, P2).
//!
//! This is the third capability operation the examples teach. Capability USE is in
//! examples/00-hello + examples/ping; capability TRANSFER is in examples/cap-grant;
//! capability MINT is here. It is the "a file is a capability" mechanism: a service asks
//! the kernel to allocate a fresh, opaque resource it owns, mints a real kernel cap for it,
//! and hands a copy to a client. The kernel mints/validates/routes/revokes the cap exactly
//! as for an endpoint cap - but it never learns what the resource MEANS. Only this service
//! does (Commandment III, §4.4). `services/fs` is this pattern grown up: its resource IS a
//! file, minted on Open and revoked on delete.
//!
//! Minting is GATED (Commandment VII, §7.10): `resource_mint` needs a RESOURCE_MINT
//! authority, granted BY NAME inside the kernel only to authorized minters like `fs` - the
//! same by-name kernel-grant mechanism examples/e1000 uses for its NIC BAR. In this plain
//! example that grant is absent, so `resource_mint` returns None and the service idles
//! (loud, bounded degradation - Commandment V). The full serve flow is written below so the
//! template is real; `fs` is the runnable proof (shell `fcap`, §22 Test 14).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};
use godspeed_sdk::capability::{RIGHT_READ, RIGHT_WRITE, RIGHT_GRANT};

// Resource operations - the FIRST payload byte of a badged invocation (mirrors fs's FOP_*).
// The kernel has already validated the cap holds the invoked right; this service additionally
// enforces that the op needs <= that right (the non-escalation check, §7.3).
const OP_READ:  u8 = 1; // needs RIGHT_READ
const OP_WRITE: u8 = 2; // needs RIGHT_WRITE
const OP_CLOSE: u8 = 4; // retire the resource: revoke it (any holder may close their handle)

// Reply codes (this service's tiny protocol; a real one is richer - see fs's FS_OK/FS_ERR).
const OK:     u8 = 0;
const DENIED: u8 = 1; // op required a right the validated cap lacked (non-escalation)

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("resource-server: starting");

    // Mint a delegated resource this service OWNS. The kernel registers a fresh opaque
    // ResourceId at generation 0, records THIS service's endpoint as its owner, and returns
    // a real cap carrying the rights we asked for. We include GRANT so we can derive a copy
    // to hand to a client (mirrors fs's `want | RIGHT_GRANT`). Minting is gated: without the
    // RESOURCE_MINT authority (granted by name in the kernel to minters like fs), this
    // returns None and we degrade gracefully.
    let (resource_id, cap) = match ctx.resource_mint(RIGHT_READ | RIGHT_WRITE | RIGHT_GRANT) {
        Some(minted) => minted,
        None => {
            ctx.log("resource-server: no RESOURCE_MINT cap (gated, §7.10) - idling. fs is the real resource server; see examples/e1000 for the by-name kernel grant.");
            ctx.park()
        }
    };
    ctx.log_fmt(format_args!(
        "resource-server: minted resource {} (we own it; the kernel tracks only the id + owner)",
        resource_id
    ));

    // Hand a client a copy of the cap. `derive_cap` duplicates it into a fresh slot; rights
    // can only NARROW on transfer, never widen (§7.3) - the copy can never out-reach the
    // original. (To issue a strictly read-only client cap, mint the resource with just
    // RIGHT_READ | RIGHT_GRANT; the client's copy then cannot write at all.) We keep the
    // owned resource (we serve it via the kernel-set badge, not the cap) and drop our copy
    // of the handed-out cap on success - authority MOVES, it does not silently duplicate.
    if let Some(copy) = ctx.derive_cap(cap) {
        match ctx.acquire_send_cap("client") {
            Some(client) => {
                let note = Message::from_bytes(b"a cap to a resource I own");
                match ctx.send_with_cap_by_handle(client, copy, &note) {
                    Ok(())  => ctx.log("resource-server: granted a resource cap to client"),
                    Err(_)  => ctx.remove_cap(copy), // send failed: reclaim the untransferred copy (no leak)
                }
            }
            None => {
                ctx.log("resource-server: no 'client' to grant to (expected when run standalone)");
                ctx.remove_cap(copy);
            }
        }
    }
    // fs drops its own minted cap after handing the client a copy - it serves the resource
    // through the unforgeable badge, never the cap. We do the same.
    ctx.remove_cap(cap);

    // Serve the resource. A holder USES its cap by invoking it (`resource_invoke`); the kernel
    // validates the cap (generation + the invoked right) and routes the message HERE, badged
    // with (resource_id, right). The badge is set ONLY by the kernel after that check, so its
    // presence is unforgeable proof of a real, live cap on a resource we own.
    ctx.log("resource-server: serving resource API");
    loop {
        let msg = ctx.recv();
        // The reply cap the kernel embedded in the invocation (so we can answer the holder).
        let reply = ctx.take_pending_cap();

        match ctx.last_recv_badge() {
            Some((rid, right)) => {
                // A kernel-validated invocation. Learn the op, then enforce op <= right: a
                // READ-validated cap must NEVER drive a WRITE (the load-bearing non-escalation
                // check, §7.3). The kernel already blocked a cap that lacked the invoked right;
                // this is the owner's matching check on the operation it is about to perform.
                let op = msg.payload_bytes().first().copied().unwrap_or(0);
                let needed = match op {
                    OP_READ  => RIGHT_READ,
                    OP_WRITE => RIGHT_WRITE,
                    _        => 0,
                };
                if op != OP_CLOSE && needed & right == 0 {
                    ctx.log("resource-server: denied - op needs a right the cap lacks (non-escalation)");
                    if let Some(r) = reply { let _ = ctx.send_by_handle(r, &Message::from_bytes(&[DENIED])); }
                    continue;
                }

                match op {
                    OP_READ | OP_WRITE => {
                        // ... act on the resource `rid` here (fs reads/writes the file it maps
                        // this id to). The kernel never learns what `rid` means - we do.
                        if let Some(r) = reply { let _ = ctx.send_by_handle(r, &Message::from_bytes(&[OK])); }
                    }
                    OP_CLOSE => {
                        // Revoke the resource: a generation bump makes EVERY outstanding cap to
                        // it go stale, so the holder's next use returns CapRevoked (§7.5/§7.10).
                        // Owner-gated by the kernel - ownership is the check. fs does this on
                        // delete/close.
                        ctx.resource_revoke(rid);
                        if let Some(r) = reply { let _ = ctx.send_by_handle(r, &Message::from_bytes(&[OK])); }
                    }
                    _ => {
                        if let Some(r) = reply { let _ = ctx.send_by_handle(r, &Message::from_bytes(&[DENIED])); }
                    }
                }
            }
            None => {
                // No badge: an ordinary name-addressed message (not a cap invocation). A real
                // server would handle its plain protocol here (fs serves Open this way).
                ctx.log("resource-server: ignoring non-badged message (no resource cap invoked)");
            }
        }
    }
}
