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

pub use godspeed_sdk::service_context::{ClockSource, Datetime};

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

/// Where the wall clock came from, so a displayed timestamp can say what it stands on.
///
/// This is the check [`datetime`] tells you to make, and it is the difference between a date and a
/// date you can quote. The four answers are genuinely different claims:
///
/// | | |
/// |---|---|
/// | [`ClockSource::Unset`] | No clock at all. Whatever [`datetime`] renders is an epoch date, not today. |
/// | [`ClockSource::Rtc`] | A local hardware clock reading a plausible date. |
/// | [`ClockSource::Ntp`] | Corrected from the network this boot. |
/// | [`ClockSource::Floor`] | A LOWER BOUND carried from the last boot - real, advancing correctly, but blind to how long the machine was off. |
///
/// `Floor` is the one worth reading twice, and it is why this is an enum rather than a bool. Several
/// of the boards this runs on have no RTC, so the clock starts from a persisted floor and creeps
/// forward correctly while being arbitrarily far behind. Reporting that as `Rtc` would claim hardware
/// the board does not have; reporting it as `Unset` would deny a time it is displaying.
///
/// **A machine that cannot be asked reads [`ClockSource::Unset`]**, deliberately. The `time` service
/// is restartable, so an unanswered question is normal during a restart - and an unknown clock and an
/// unset clock oblige a caller to do the same thing, which is not to quote the date as fact.
pub fn clock_source(ctx: &ServiceContext) -> ClockSource {
    // OP_NOW -> [ok, epoch(8), source]. Bounded, and reacquiring: see `crate::call::request_within`.
    const OP_NOW: u8 = 1;
    let reply = match crate::call::request_within(
        ctx, "time", &godspeed_sdk::ipc::Message::from_bytes(&[OP_NOW]), 2) {
        Ok(r) => r,
        Err(_) => return ClockSource::Unset,
    };
    let p = reply.payload_bytes();
    if p.len() < 10 || p[0] == 0 {
        return ClockSource::Unset;
    }
    match p[9] {
        1 => ClockSource::Rtc,
        2 => ClockSource::Ntp,
        3 => ClockSource::Floor,
        _ => ClockSource::Unset,
    }
}

/// Whether [`datetime`] is worth showing as a date at all.
///
/// True for [`ClockSource::Rtc`] and [`ClockSource::Ntp`]. False for [`ClockSource::Unset`], and
/// false for [`ClockSource::Floor`] - a floor is a real lower bound but not a reading, so a program
/// that only wants to know "may I print this as today's date" should treat it as no.
///
/// Reach for [`clock_source`] when the distinction matters, which it does more often than this
/// shorthand suggests.
pub fn clock_is_set(ctx: &ServiceContext) -> bool {
    matches!(clock_source(ctx), ClockSource::Rtc | ClockSource::Ntp)
}

/// Which core this service is running on.
///
/// For reporting, not for deciding. Placement is the supervisor's (CLAUDE.md 9.2), a service never
/// migrates while it runs, and it may be placed on a different core after a restart - so a service
/// that CHANGES BEHAVIOUR based on this has coupled itself to a deployment detail that is explicitly
/// allowed to move (invariant 11).
pub fn core_id(ctx: &ServiceContext) -> u32 {
    ctx.core_id()
}
