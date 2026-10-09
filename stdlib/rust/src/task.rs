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

/// Sleep for one scheduler quantum, parked: the shortest sleep there is.
///
/// Not [`yield_now`]. A yield returns at once when nothing else is runnable, so a loop built on it
/// spins a core; this parks the task until the next tick (CLAUDE.md 9.1) - on every machine but a
/// real Pi 2, where it currently sleeps about a microsecond (see the note in the body). For a
/// loop that has nothing to wait on but must not hammer the CPU between looks.
pub fn sleep_quantum(ctx: &ServiceContext) {
    // One counter tick, which the kernel floors to the next scheduler tick: one quantum, calibrated or
    // not - EXCEPT on a real Pi 2 (known defect, 2026-10-09). Its counter runs at 1 MHz, so one tick
    // converts to 1 us, which the ARMv7 sub-tick sleep path (`handle_sleep`) takes, and this returns
    // after about a microsecond instead of a quantum. (QEMU's raspi2b counter is 62.5 MHz, where one
    // tick rounds to 0 us and the tick path is taken.)
    ctx.sleep(1);
}

/// Sleep for `us` microseconds, at the kernel's resolution.
///
/// The kernel's sleep ends on a scheduler tick on every port but ARMv7 (the Pi 2), which has a
/// sub-tick timer for sleeps under a quantum and falls back to the tick when its few slots are taken;
/// elsewhere anything under a quantum becomes a quantum. This is for a pace written in microseconds,
/// not a hardware hold - a hold that must not be cut short or
/// stretched belongs to `gs::driver::delay`. On a machine whose clock the kernel could not calibrate
/// it is one quantum, the same floor [`sleep_ms`] has there.
pub fn sleep_us(ctx: &ServiceContext, us: u64) {
    let per_10ms = ctx.tsc_ticks_per_10ms();
    ctx.sleep(if per_10ms == 0 { 1 } else { crate::driver::wait::ticks_for(per_10ms, us) });
}

/// Sleep for `ticks` of the counter [`crate::driver::wait::ticks`] reads.
///
/// For code that already works in counter ticks - a driver that measured how long the kernel's sleep
/// really lasts on this board and schedules against that measurement. Anything written in time uses
/// [`sleep_ms`] or [`sleep_us`], which convert for you and know what an uncalibrated machine means;
/// a hand-converted tick count here is the copy those exist to replace. Zero is treated as one: a
/// sleep of nothing is a yield, and [`yield_now`] says so.
pub fn sleep_ticks(ctx: &ServiceContext, ticks: u64) {
    ctx.sleep(ticks.max(1));
}

/// Seconds since this machine booted.
///
/// Monotonic and always available, including before any clock is set, which is what makes it the
/// right basis for "how long has this taken" - unlike the wall clock, which can be unset or can jump
/// when the wall clock is learned.
pub fn uptime_secs(ctx: &ServiceContext) -> i64 {
    ctx.uptime_secs()
}

/// Seconds that never go backwards (InspectKernel query 17).
///
/// For stamping a sequence of events where ordering matters more than absolute accuracy. **The
/// origin depends on the board**: on x86 it is the CMOS RTC's epoch seconds with backward and
/// wild-forward readings dropped; on the boards with no RTC (the Pis, the VisionFive) it is seconds
/// since boot. Use it for differences, never as a date.
pub fn epoch_secs_monotonic(ctx: &ServiceContext) -> i64 {
    ctx.epoch_secs_monotonic()
}

/// The HARDWARE real-time clock as calendar fields (InspectKernel query 11), with
/// [`Datetime::epoch_secs`] to turn it into epoch seconds.
///
/// **This is the RTC, not the `time` service's clock.** The wall clock left the kernel for the
/// `time` service, which corrects it from the network or carries a floor across boots; none of
/// that reaches this call. So [`clock_source`] can answer [`ClockSource::Ntp`] or
/// [`ClockSource::Floor`] while this still reads the raw hardware.
///
/// **A machine may not have one.** On the boards with no RTC (the Pis, the VisionFive) the kernel
/// answers 0, which unpacks to all-zero fields - year 0, not today and not 1970 - and stays that
/// way after the network has supplied the time. On x86 it is the CMOS clock, unchecked. Check
/// [`clock_source`] rather than printing this; [`uptime_secs`] is the one that is always
/// meaningful.
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
/// | [`ClockSource::Unset`] | No clock at all. Whatever [`datetime`] renders is not today. |
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

/// Whether the `time` service's clock is worth showing as a date at all.
///
/// Says nothing about [`datetime`] on a board with no RTC: there `Ntp` can be true while
/// [`datetime`] still reads zeros (see its doc).
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
