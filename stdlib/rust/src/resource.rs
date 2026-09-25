// SPDX-License-Identifier: Apache-2.0
//! Invoking a delegated resource capability, once, for everything that has one.
//!
//! `fs` mints file capabilities and `net-stack` mints socket, listener and connection capabilities.
//! They are the same mechanism (CLAUDE.md 7.10) with different payloads, so the parts that are easy
//! to get subtly wrong live here rather than once per module:
//!
//! - **the reply cap is one-shot and per-invocation**, derived from our own endpoint and consumed by
//!   the kernel on delivery. Reclaim it ONLY where the send never delivered - `8.5` removes an
//!   embedded cap from the sender's table, so removing it again removes whatever the kernel has since
//!   put in that slot. That is how a file capability was once deleted out from under its holder.
//! - **a message that is not our reply is HELD, never dropped.** A caller waits on its own ordinary
//!   endpoint - the same one its clients send to - because a task has exactly one. Draining would
//!   destroy real client requests; that is what `services/shell` does, and it is safe only because
//!   the shell serves nobody.
//! - **the kernel's refusal is READ, not inferred.** An earlier cut guessed from the rights mask and
//!   reported a revoked capability as "could not be reached", which named the wrong fault and sent an
//!   operator to check a service that was running perfectly.

use godspeed_sdk::capability::{CapError, CapHandle};
use godspeed_sdk::ipc::{IpcError, Message};
use godspeed_sdk::service_context::{ReqOutcome, ServiceContext};

use crate::cap::Cap;
use crate::error::Error;

/// How many messages that are NOT our reply one invocation will hold before it stops taking them.
///
/// Two, and small on purpose: a held message is a full 4 KiB `Message`, so the footprint stays
/// readable from this line (26.6.1). Two covers a client or two arriving during one round trip;
/// beyond it the operation stops rather than dropping anything.
pub const HELD_MAX: usize = 2;

/// The largest request body one invocation carries, plus its tag.
///
/// Sized for the biggest user, which is a filesystem chunk. A datagram is far smaller and pays the
/// same fixed cost - the alternative is a second constant and a second buffer shape, which is a
/// worse trade than a few unused kilobytes of stack.
const REQ_MAX: usize = 16 + crate::fs::IO_CHUNK;

/// Messages that arrived during an invocation and were not its reply.
pub(crate) struct Held {
    msgs: [Option<Message>; HELD_MAX],
    n: usize,
}

impl Held {
    pub(crate) const fn new() -> Held {
        Held { msgs: [None, None], n: 0 }
    }

    /// Take the oldest held message, or `None`.
    ///
    /// FIFO: handing them back out of order would have a client's two requests answered backwards.
    pub(crate) fn take(&mut self) -> Option<Message> {
        if self.n == 0 {
            return None;
        }
        let first = self.msgs[0].take();
        for i in 1..HELD_MAX {
            self.msgs[i - 1] = self.msgs[i].take();
        }
        self.n -= 1;
        first
    }
}

/// Send `[tag, body..]` through a resource capability and wait for the reply carrying `tag`.
///
/// Returns the reply WHOLE, tag still at byte 0 - the caller reads its own body from index 1.
/// Stripping would mean rebuilding a 4 KiB `Message` on every call, and the one time this library
/// pretended it had stripped, two call sites read the status byte as data.
pub(crate) fn invoke(
    ctx: &ServiceContext,
    cap: CapHandle,
    right: u8,
    tag: u8,
    patience: Option<u8>,
    body: &[u8],
    secs: i64,
    held: &mut Held,
) -> Result<Message, Error> {
    // The header is built HERE rather than by each caller, so nobody has to keep a second buffer to
    // prepend one byte to - this function already owns a `REQ_MAX` one.
    //
    // `patience` is how many seconds the caller is about to wait. A service that has to put this
    // request aside while it talks to something else can then hold it for as long as it is actually
    // worth answering, instead of against a constant that cannot know. `None` is for an owner that
    // does no such holding (`fs`), and costs a byte nothing would read.
    let hdr = 1 + patience.is_some() as usize;
    if body.len() + hdr > REQ_MAX {
        return Err(Error::InvalidInput);
    }
    let mut req = [0u8; REQ_MAX];
    req[0] = tag;
    if let Some(p) = patience {
        req[1] = p;
    }
    let body_end = hdr + body.len();
    req[hdr..body_end].copy_from_slice(body);

    let self_grant = ctx.self_grant_handle().ok_or(Error::Unreachable)?;
    let reply_cap = ctx.derive_cap(self_grant).ok_or(Error::Busy)?;

    if let Err(e) = ctx.resource_invoke(cap, right, reply_cap, &Message::from_bytes(&req[..body_end])) {
        // Refused before routing, so the reply cap was NOT consumed: reclaim the slot (8.5).
        ctx.remove_cap(reply_cap);
        return Err(match e {
            IpcError::CapError(CapError::CapInsufficientRights)
            | IpcError::CapError(CapError::CapNotGrantable)
            | IpcError::CapError(CapError::CapWrongScope) => Error::PermissionDenied,
            // Revoked, or the issuer died and was replaced. One meaning to a holder: this capability
            // is finished, and the answer is to re-open rather than to retry.
            IpcError::CapError(CapError::CapRevoked)
            | IpcError::CapError(CapError::CapNotHeld)
            | IpcError::CapError(CapError::EndpointDead)
            | IpcError::EndpointDead
            | IpcError::ReplyDead => Error::Revoked,
            _ => Error::Unreachable,
        });
    }

    loop {
        match ctx.recv_abortable_deadline(secs) {
            ReqOutcome::Reply(m) => {
                if m.payload_bytes().first() == Some(&tag) {
                    return Ok(m);
                }
                // NOT OURS. Holding it is the entire reason this is safe to hand to a service that
                // serves other clients.
                if held.n == HELD_MAX {
                    // No room. Stop taking, so the rest stays queued for the caller. Whether the
                    // operation completed is genuinely unknown - say exactly that.
                    return Err(Error::OutcomeUnknown);
                }
                held.msgs[held.n] = Some(m);
                held.n += 1;
            }
            // The caller's own abort. Not a fault.
            ReqOutcome::Aborted => return Err(Error::Cancelled),
            // The deadline passed. The request LEFT, so it may have been performed.
            ReqOutcome::Timeout => return Err(Error::OutcomeUnknown),
        }
    }
}

// ── The owner's side: minting and revoking a resource of your own ──────────────────────────────
//
// Everything above serves a HOLDER using someone else's capability. These three are for a service
// that ISSUES them - what `fs` does for a file and `net-stack` for a socket.

/// Mint a fresh resource capability that THIS service owns (CLAUDE.md 7.10).
///
/// Returns the opaque `ResourceId` the kernel will badge invocations with, and a capability carrying
/// `rights` that you can hand to a client. The kernel tracks the id and the owning endpoint and
/// nothing else - **what the resource MEANS is yours alone**, which is what keeps filesystem logic
/// out of the kernel (CLAUDE.md 4.4).
///
/// Gated: minting requires the `RESOURCE_MINT` authority, granted only to services that legitimately
/// issue resources. Delegated minting is explicit authority, never ambient (CLAUDE.md 3.1).
pub fn mint(ctx: &ServiceContext, rights: u8) -> Result<(u64, Cap), Error> {
    match ctx.resource_mint(rights) {
        Some((id, h)) => Ok((id, Cap::from_handle(h))),
        None => Err(Error::PermissionDenied),
    }
}

/// Revoke a resource this service owns, invalidating EVERY capability to it.
///
/// One generation bump, and every outstanding copy goes stale at once - there is no list of holders
/// to walk and none can be missed (CLAUDE.md 7.5). `fs` does this on delete and on close.
///
/// Returns `false` if this service does not own that id. Nothing is revoked in that case, which is
/// worth reporting rather than treating as done.
pub fn revoke(ctx: &ServiceContext, resource_id: u64) -> bool {
    ctx.resource_revoke(resource_id)
}

/// The resource id and right the kernel badged the message just received with.
///
/// `None` means the message was an ordinary name-addressed request rather than a capability
/// invocation. The badge is set BY THE KERNEL after it validated the capability, so it cannot be
/// forged by a client sending over its ordinary send capability - which is what makes it safe to act
/// on. Enforce `op <= right` yourself: the kernel checked the capability, not what you do with it.
pub fn last_badge(ctx: &ServiceContext) -> Option<(u64, u8)> {
    ctx.last_recv_badge()
}
