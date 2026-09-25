// SPDX-License-Identifier: Apache-2.0
//! This service's own execution: giving up the CPU, waiting, and what time it is.
//!
//! # Waiting is not the same as sleeping
//!
//! The functions here wait on the CLOCK, and that makes them the wrong tool for waiting on another
//! service. Commandment VIII: wait on the truth, not on time. If you are sleeping because a peer
//! might not be ready yet, you have written a race that passes on your machine - wait for the thing
//! itself ([`crate::ipc::recv_within`], a reply, an interrupt), and let it tell you.
//!
//! What a sleep IS for: pacing something that has no event to wait on. A poll of an external device,
//! a retry after congestion, a display that should not repaint faster than anyone can read.

use godspeed_sdk::service_context::ServiceContext;

pub use godspeed_sdk::service_context::Datetime;

/// Give up the rest of this quantum.
///
/// Advisory only: the scheduler preempts at 10 ms regardless (CLAUDE.md 9.3), so this is politeness
/// rather than a correctness tool. It cannot be relied on to let another service run, and a loop that
/// needs it to make progress is a loop that should be blocking on something.
pub fn yield_now(ctx: &ServiceContext) {
    ctx.yield_cpu();
}

/// Sleep for `ms` milliseconds.
///
/// Read the module header before reaching for this: if something else could tell you when to wake,
/// waiting for THAT is both faster and correct on a machine you have not tried yet.
pub fn sleep_ms(ctx: &ServiceContext, ms: u64) {
    ctx.sleep_ms(ms);
}

/// Seconds since this machine booted.
///
/// Monotonic and always available, including before any clock is set, which is what makes it the
/// right basis for "how long has this taken" - unlike the wall clock, which can be unset or can jump
/// when the wall clock is learned.
pub fn uptime_secs(ctx: &ServiceContext) -> i64 {
    ctx.uptime_secs()
}

/// Epoch seconds that never goes backwards, even if the wall clock is corrected.
///
/// For stamping a sequence of events where ordering matters more than absolute accuracy.
pub fn epoch_secs_monotonic(ctx: &ServiceContext) -> i64 {
    ctx.epoch_secs_monotonic()
}

/// The current wall clock as calendar fields, and the only route to epoch seconds
/// ([`Datetime::epoch_secs`]).
///
/// **A machine may not know the time.** Several of the boards this runs on have no RTC, so the wall
/// clock is learned from the network and may never arrive. On a machine whose clock was never set
/// this renders an epoch date - a true statement about the clock and a false one about today - so
/// check it rather than printing it. [`uptime_secs`] is the one that is always meaningful.
pub fn datetime(ctx: &ServiceContext) -> Datetime {
    ctx.datetime()
}
