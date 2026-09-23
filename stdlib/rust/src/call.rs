// SPDX-License-Identifier: Apache-2.0
//! One way to ask a service for something, and get an honest answer.
//!
//! # Why this exists
//!
//! The SDK offers eight `request_with_reply*` functions across three outcome enums. They are all
//! correct; the problem is which one is shortest to type. `request_with_reply_deadline` returns
//! `Option<Message>`, so "the request never left" and "the deadline passed" arrive as the same
//! `None` - and those two demand opposite responses. Five services
//! (`block-driver`, `control`, `copier`, `nic-driver`, `recorder`) reached for the longer
//! `_call_err` variant to get the distinction back.
//!
//! This module offers ONE function. It keeps the distinction, and it does the one retry that is
//! provably safe, so that the easy path and the correct path are the same path.
//!
//! # What it does on your behalf, and what it refuses to
//!
//! ```text
//!   send ──▶ Reply          you get the message
//!        │
//!        ├─▶ SendFailed ──▶ REACQUIRE the peer by name, send ONCE more
//!        │                  (§14.3: a cap to a restarted service is stale forever,
//!        │                   and the obligation to notice is the client's)
//!        │                     └─▶ still failing: Err(Unreachable)  nothing happened
//!        │
//!        ├─▶ QueueFull ────▶ Err(Busy)            nothing happened; pace yourself
//!        │
//!        └─▶ Timeout ──────▶ Err(OutcomeUnknown)  IT MAY HAVE HAPPENED
//! ```
//!
//! **It never retries a timeout.** That is not caution, it is correctness: a slow service is a live
//! service, and re-sending a `delete` to one is not a retry, it is a second delete whose failure
//! looks like success.
//!
//! # Authority
//!
//! Every call takes `&ServiceContext`. There is no global, no ambient handle, and no way to reach a
//! service this task's contract did not grant. If the capability was never granted the call returns
//! `Unreachable`, the same as any other peer this task cannot reach - it does not acquire one.

use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::{DeadlineOutcome, ServiceContext};

use crate::error::Error;

/// The SDK's outcome, carried across WITHOUT reinterpretation.
///
/// One-to-one, deliberately. `SendFailed` and `Timeout` must never collapse into a single value:
/// that merge is exactly what `request_with_reply_deadline`'s `Option` performs, and what five
/// services reached for a longer-named function to undo.
///
/// This lives here rather than as a `From` impl on `Error` so that `error.rs` stays free of any SDK
/// import and remains host-unit-testable - the same shared-pure-module pattern `kernel/src/clock.rs`
/// documents. The mapping itself is exercised on the target by `osdev test stdlib`.
fn outcome_to_error(o: DeadlineOutcome) -> Error {
    match o {
        DeadlineOutcome::SendFailed => Error::Unreachable,
        DeadlineOutcome::QueueFull  => Error::Busy,
        DeadlineOutcome::Timeout    => Error::OutcomeUnknown,
        // A `Reply` reaching here would be a bug in this module, not a failure of the peer.
        DeadlineOutcome::Reply(_)   => Error::Malformed,
    }
}

/// How long to wait before calling an answer late, when the caller does not say.
///
/// Five seconds is the figure `services/copier` uses for a single filesystem chunk, chosen because
/// it is comfortably longer than any healthy answer and short enough that a wedged peer does not
/// hold a utility forever. An operation that legitimately takes longer (a whole-volume scrub) must
/// say so with [`request_within`] rather than raising this for everyone.
pub const DEFAULT_SECS: i64 = 5;

/// Ask `peer` for something and wait up to [`DEFAULT_SECS`] for the answer.
///
/// See [`request_within`]; this is that, with the common deadline.
pub fn request(ctx: &ServiceContext, peer: &str, msg: &Message) -> Result<Message, Error> {
    request_within(ctx, peer, msg, DEFAULT_SECS)
}

/// Ask `peer` for something and wait up to `secs` for the answer.
///
/// **Blocks** until the reply arrives or the deadline passes. **Not cancellable**: the deadline is
/// the only bound, which is why it is a parameter rather than a constant.
///
/// **Authority:** uses this task's existing send capability for `peer`. It never creates one.
///
/// **If the peer restarts** while the request is in flight, the send fails rather than vanishing,
/// and this reacquires the peer by name and sends once more - so an ordinary restart is invisible
/// to the caller, which is what §14.3 asks of every client. If the peer restarts AFTER receiving
/// the request, the deadline passes instead and you get [`Error::OutcomeUnknown`], because that is
/// the truth.
///
/// # Errors
///
/// - [`Error::Unreachable`] - the request never left, twice. Nothing happened.
/// - [`Error::Busy`] - the peer's queue is full. Nothing happened.
/// - [`Error::OutcomeUnknown`] - no reply in time. **It may have happened.**
pub fn request_within(
    ctx: &ServiceContext, peer: &str, msg: &Message, secs: i64,
) -> Result<Message, Error> {
    match ctx.request_with_reply_deadline_outcome(peer, msg, secs) {
        DeadlineOutcome::Reply(r) => Ok(r),

        // THE ONE RETRY THAT IS SAFE. The send did not happen, so re-sending is the same
        // operation rather than a second one. `reacquire_cap` is what makes the retry worth
        // doing: without it the second send uses the same stale handle and fails identically.
        DeadlineOutcome::SendFailed => {
            if ctx.reacquire_cap(peer).is_err() {
                return Err(Error::Unreachable);
            }
            match ctx.request_with_reply_deadline_outcome(peer, msg, secs) {
                DeadlineOutcome::Reply(r)  => Ok(r),
                DeadlineOutcome::Timeout   => Err(Error::OutcomeUnknown),
                other                      => Err(outcome_to_error(other)),
            }
        }

        // NOT retried here, deliberately. Congestion clears on its own and the caller knows its own
        // pacing; a library that retries a full queue on your behalf turns one late request into
        // two and calls it help.
        DeadlineOutcome::QueueFull => Err(Error::Busy),

        // NEVER retried. See the module header.
        DeadlineOutcome::Timeout => Err(Error::OutcomeUnknown),
    }
}
