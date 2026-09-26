// SPDX-License-Identifier: Apache-2.0
//! One way to ask a service for something, and get an honest answer.
//!
//! # Why this exists
//!
//! The SDK offers twelve `request_with_reply*` functions across three outcome enums. They are all
//! correct; the problem is which one is shortest to type. `request_with_reply_deadline` returns
//! `Option<Message>`, so "the request never left" and "the deadline passed" arrive as the same
//! `None` - and those two demand opposite responses. Three services
//! (`block-driver`, `control`, `nic-driver`) reach for the longer `_call_err` variant to get the
//! distinction back. (This said five, naming `copier` and `recorder` too; both have since migrated to
//! `gs::fs` / `gs::call` and make no `request_with_reply*` call at all.)
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
//! # ONE OF THESE IS SAFE FOR A SERVING CALLER, AND THE OTHER IS NOT
//!
//! Read this before picking. The difference is not ergonomic, it decides whether a task that also
//! SERVES clients can lose one of their requests (CLAUDE.md 8.2).
//!
//! - [`request_within`] rides `CallDeadline`. The kernel matches the reply to this call's own
//!   one-shot reply capability and leaves every other message queued. **Safe for a serving task.**
//! - [`request_within_notice`] rides the SDK's `_qhint` variant, which DRAINS the endpoint before
//!   sending (`while self.try_recv().is_some() {}`) and then waits with a plain timed receive, taking
//!   whatever lands next. **NOT safe for a serving task**: a client request arriving mid-wait is
//!   consumed and dropped.
//!
//! That is deliberate rather than an oversight, and the SDK says so where it lives: `_qhint`
//! interleaves ON PURPOSE so it can notice a `q` keypress while waiting, and making it dequeue only
//! the reply would delete that. The honest fix is a bounded stash, which is real work and is not
//! done - so the limitation is recorded here (26.7) instead of being implied away.
//!
//! **It reaches further than this module.** Every `Fs::call` goes through
//! [`request_within_notice`](request_within_notice), and so does every `Net` call, so a handle built
//! with `Fs::with_notice` or `Net::with_notice` is on that path too. Use the plain constructors in a
//! service that serves; `with_notice` is for a foreground command where a person is waiting and may
//! want to press `q`.
//!
//! # Authority
//!
//! Every call takes `&ServiceContext`. There is no global, no ambient handle, and no way to reach a
//! service this task's contract did not grant. If the capability was never granted the call returns
//! `Unreachable`, the same as any other peer this task cannot reach - it does not acquire one.

use godspeed_sdk::ipc::{IpcError, Message};
use godspeed_sdk::service_context::{ReqOutcome, ServiceContext};

use crate::error::Error;

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

/// How long a wait must last before the caller is told it is lasting.
///
/// Two seconds is the shell's own figure. Short enough that a person does not decide the machine is
/// dead; long enough that an ordinary answer never triggers it.
pub const NOTICE_AFTER_SECS: i64 = 2;

/// [`request_within`], plus a callback fired once if the wait drags on.
///
/// # Why this exists, given the rest of this module is about not being clever
///
/// `services/shell` prints `[q] quit` when a network request lingers, so a person waiting on an
/// unreachable host can stop it rather than wonder. That is an interactive affordance and it is
/// exactly the sort of thing a standard library should NOT own - which is why the callback takes no
/// arguments and returns nothing. It means only **"you have been waiting a while"**. What to do
/// about it - print, poll a key, set a flag - stays entirely with the caller, and this module keeps
/// knowing nothing about consoles.
///
/// Without it the shell cannot move onto this library without losing that affordance, which would
/// have been a real regression dressed up as a migration.
pub fn request_within_notice(
    ctx: &ServiceContext, peer: &str, msg: &Message, secs: i64, notice: Option<&dyn Fn()>,
) -> Result<Message, Error> {
    let notice = match notice {
        None => return request_within(ctx, peer, msg, secs),
        Some(f) => f,
    };
    match ctx.request_with_reply_qhint(peer, msg, NOTICE_AFTER_SECS, secs, || notice()) {
        ReqOutcome::Reply(r) => Ok(r),
        // `ReqOutcome` does not separate a failed send from a passed deadline the way
        // `DeadlineOutcome` does, so the retry `request_within` performs cannot be done safely here:
        // retrying a timeout is the one thing this module refuses. Reported as unknown, which is the
        // honest reading of the coarser answer.
        ReqOutcome::Timeout => Err(Error::OutcomeUnknown),
        // The caller stopped waiting. Reported as ITSELF, not folded into the timeout: a person
        // pressing `q` and a service going silent are different facts and a caller will say
        // different things about them. Collapsing those two would be the same mistake this module
        // exists to stop the SDK's `Option` making.
        ReqOutcome::Aborted => Err(Error::Cancelled),
    }
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
    // `CallDeadline`, NOT a send followed by a plain recv.
    //
    // The kernel matches the reply to the one-shot reply capability this carries and leaves every
    // other message queued. That is the difference between a library a SERVING task can call and one
    // it cannot: a plain recv takes whatever lands next, so a client request that arrives mid-wait is
    // consumed, misparsed, and lost. CLAUDE.md 8.2's amendment adds the primitive for exactly this,
    // and `services/recorder` demonstrated the gap the day it stopped using it - a capture that died
    // because the shell asked it a question at the wrong moment.
    //
    // There is nothing to hand back to the caller afterwards, either: the unrelated message was
    // never taken, so it is still in the queue for the caller's own loop.
    match ctx.request_with_reply_call_err(peer, msg, secs) {
        Ok(Some(r)) => Ok(r),

        // NEVER retried. See the module header.
        Ok(None) => Err(Error::OutcomeUnknown),

        // NOT retried, deliberately. Congestion clears on its own and the caller knows its own
        // pacing; a library that retries a full queue on your behalf turns one late request into
        // two and calls it help.
        Err(IpcError::QueueFull) => Err(Error::Busy),

        // THE ONE RETRY THAT IS SAFE. The send did not happen, so re-sending is the same
        // operation rather than a second one. `reacquire_cap` is what makes the retry worth
        // doing: without it the second send uses the same stale handle and fails identically.
        Err(_) => {
            if ctx.reacquire_cap(peer).is_err() {
                return Err(Error::Unreachable);
            }
            match ctx.request_with_reply_call_err(peer, msg, secs) {
                Ok(Some(r))              => Ok(r),
                Ok(None)                 => Err(Error::OutcomeUnknown),
                Err(IpcError::QueueFull) => Err(Error::Busy),
                Err(_)                   => Err(Error::Unreachable),
            }
        }
    }
}
