// SPDX-License-Identifier: Apache-2.0
//! Messages between services: sending one, receiving one, and what the failures mean.
//!
//! # The model in four sentences
//!
//! A service owns one endpoint with a bounded queue (16 messages, CLAUDE.md 8.5). A send COPIES the
//! bytes into the receiver's queue and returns; it does not wait for the receiver to look at them. So
//! a successful [`send`] means **queued, not processed** - if you need to know it was acted on, the
//! protocol has to say so, which is what [`crate::call`] is for. And a message can carry a
//! capability, which MOVES: the sender no longer holds it afterwards.
//!
//! # Why every failure here is worth reading rather than retrying
//!
//! The three you will actually meet say different things and want different answers:
//!
//! - [`Error::Unreachable`](crate::Error) - the peer died, or its name does not resolve right now.
//!   **Nothing happened.** The name is stable even though the instance is not (CLAUDE.md invariant
//!   11), so the answer is [`cap::reacquire`](crate::cap::reacquire) and then retry - not a sleep.
//! - [`Error::Busy`](crate::Error) - the peer is alive and its queue is full. **Nothing happened.**
//!   Congestion is transient; pace and retry, and do not go looking for a peer that never went away.
//! - [`Error::InvalidInput`](crate::Error) - the message exceeds one page. Nothing will make that
//!   work; send less.
//!
//! A fourth appears only when you hand a capability away with [`send_granting`]:
//!
//! - [`Error::PermissionDenied`](crate::Error) - the capability you tried to send does not carry
//!   [`GRANT`](crate::cap::GRANT). **Nothing moved, so it is still yours** and the slot is still
//!   yours to reclaim. This is the first of the three checks CLAUDE.md 8.5 asks of every transfer,
//!   and it is the one a compiler cannot make for you.
//!
//! # The deadlock rule, which the kernel will not save you from
//!
//! If A and B both send to each other, at least one direction MUST use [`try_send`]. A blocking send
//! into a full queue waits, and two services waiting on each other wait forever. The kernel does not
//! detect this and does not break it (CLAUDE.md 8.9) - the supervisor's starvation watchdog is a last
//! resort, not a design.

use godspeed_sdk::ipc::IpcError;
use godspeed_sdk::service_context::ServiceContext;

pub use godspeed_sdk::ipc::Message;

use crate::cap::Cap;
use crate::error::Error;

/// The largest message body, in bytes: one page (CLAUDE.md 8.5).
///
/// Bigger than this is not a tuning problem, it is a protocol problem - stream it in pieces, the way
/// [`crate::fs`] does, rather than looking for a way to raise the limit.
pub const MAX_BYTES: usize = godspeed_sdk::ipc::MAX_PAYLOAD;

/// Translate a transport failure into the one error type a program handles.
///
/// The mapping is about what the CALLER should do rather than about which syscall said no: to a
/// sender, a dead endpoint and a revoked capability are one situation - the message never left -
/// and a full queue is a different one.
///
/// **This deliberately differs from [`crate::resource`], and the difference is load-bearing.** An
/// invocation there reports a dead or revoked capability as [`Error::Revoked`], whose
/// [`retry_is_safe`](crate::Error::retry_is_safe) is `false`, because the capability itself must be
/// re-opened before a retry means anything. A send reports [`Error::Unreachable`], whose
/// `retry_is_safe` is `true`, because reacquiring the NAME and sending again is exactly the right
/// move (CLAUDE.md invariant 11). Same kernel error, different obligation on the caller.
fn from_ipc(e: IpcError) -> Error {
    use godspeed_sdk::capability::CapError;
    match e {
        IpcError::QueueFull => Error::Busy,
        IpcError::MessageTooLarge => Error::InvalidInput,
        IpcError::CapError(CapError::CapInsufficientRights)
        | IpcError::CapError(CapError::CapNotGrantable)
        | IpcError::CapError(CapError::CapWrongScope) => Error::PermissionDenied,
        _ => Error::Unreachable,
    }
}

/// Send to a service by NAME, blocking if its queue is full.
///
/// Returns when the message is queued. That is not the same as handled - see the module header.
///
/// Blocking is the right default for a client talking to a server it does not also serve. If the
/// peer can send to YOU, use [`try_send`] in at least one direction (CLAUDE.md 8.9).
pub fn send(ctx: &ServiceContext, peer: &str, msg: &Message) -> Result<(), Error> {
    ctx.send(peer, msg).map_err(from_ipc)
}

/// Send to a service by NAME, refusing rather than waiting if its queue is full.
///
/// [`Error::Busy`](crate::Error) means the message was NOT queued and nothing happened. This is the
/// direction that breaks a mutual-send deadlock, and the one a server uses to answer a client.
pub fn try_send(ctx: &ServiceContext, peer: &str, msg: &Message) -> Result<(), Error> {
    ctx.try_send(peer, msg).map_err(from_ipc)
}

/// Send through a capability you already hold, rather than resolving a name.
///
/// Faster and more honest than a name lookup when you have the capability: authority is the thing you
/// hold, and this says so at the call site.
pub fn send_to(ctx: &ServiceContext, cap: Cap, msg: &Message) -> Result<(), Error> {
    ctx.send_by_handle(cap.handle(), msg).map_err(from_ipc)
}

/// Non-blocking [`send_to`].
pub fn try_send_to(ctx: &ServiceContext, cap: Cap, msg: &Message) -> Result<(), Error> {
    ctx.try_send_by_handle(cap.handle(), msg).map_err(from_ipc)
}

/// Send a message that CARRIES a capability, giving it away.
///
/// The capability MOVES (CLAUDE.md 8.5). Three things are true every time and none of them are
/// checked for you at compile time:
///
/// 1. The capability must hold [`GRANT`](crate::cap::GRANT), or this is refused.
/// 2. On success it is **no longer yours**. Using it afterwards is using something you do not hold.
/// 3. On failure it **stayed** - reclaim the slot with [`cap::remove`](crate::cap::remove) rather
///    than leaking it.
///
/// So the result is never ignorable: it decides what you may do next.
pub fn send_granting(
    ctx: &ServiceContext,
    to: Cap,
    granting: Cap,
    msg: &Message,
) -> Result<(), Error> {
    ctx.send_with_cap_by_handle(to.handle(), granting.handle(), msg)
        .map_err(from_ipc)
}

/// Block until a message arrives on this service's endpoint.
///
/// This takes whatever is next. If this service also awaits REPLIES on the same endpoint - which it
/// does, because a task owns exactly one - a plain receive can swallow an unrelated client's request
/// and drop it. That is not hypothetical; it cost this project a day (CLAUDE.md 8.2). When you are
/// waiting for an answer to something you asked, use [`crate::call`], which matches the reply to its
/// own reply capability and leaves everything else queued.
pub fn recv(ctx: &ServiceContext) -> Message {
    ctx.recv()
}

/// Take a message if one is already waiting, without blocking.
///
/// `None` means the queue was empty at that instant and nothing more.
pub fn try_recv(ctx: &ServiceContext) -> Option<Message> {
    ctx.try_recv()
}

/// Block for a message, giving up after `secs`.
///
/// `Ok(None)` is the deadline passing - a fact about time, not a failure of the peer. Prefer this to
/// a bare [`recv`] anywhere a missing message would otherwise hang the service forever, which is
/// every place a peer can die (CLAUDE.md 26.6).
pub fn recv_within(ctx: &ServiceContext, secs: i64) -> Option<Message> {
    ctx.recv_timeout(ctx.duration_cycles((secs.max(0) as u64) * 1000))
}

/// The capability that arrived embedded in the message just received, if any.
///
/// Call it directly after the receive that carried it: it reports the LAST receive, so anything in
/// between takes its place. A request/reply server calls this to get the reply capability its client
/// sent (CLAUDE.md 8.2).
pub fn take_sent_cap(ctx: &ServiceContext) -> Option<Cap> {
    ctx.take_pending_cap().map(Cap)
}

/// Sleep forever, having nothing left to do.
///
/// For a service whose work is finished but whose death would be noise - it holds its slot and its
/// capabilities and consumes no CPU. A service that should be gone should exit instead.
pub fn park(ctx: &ServiceContext) -> ! {
    ctx.park()
}

/// A capability to a peer this service was WIRED TO at spawn, by position.
///
/// Not a name lookup. These are the capabilities the supervisor installed from the contract's
/// `ipc_send` list before this service ran, and position `0` is the first of them. A pipe stage
/// reaches the next stage this way, which is what lets it send downstream while holding authority to
/// reach nothing else (CLAUDE.md appendix D.3).
///
/// `None` means no peer was wired at that position - the contract did not ask for one, or the
/// composition that would have supplied it did not happen.
pub fn peer_at(ctx: &ServiceContext, idx: usize) -> Option<Cap> {
    ctx.send_peer_at(idx).map(Cap::from_handle)
}

/// A capability to a peer this service was wired to at spawn, by name.
///
/// The same set as [`peer_at`], addressed by the name in the contract rather than by position. Use
/// this when the contract names several peers and position would be a guess.
pub fn peer(ctx: &ServiceContext, name: &str) -> Option<Cap> {
    ctx.send_peer_handle(name).map(Cap::from_handle)
}
