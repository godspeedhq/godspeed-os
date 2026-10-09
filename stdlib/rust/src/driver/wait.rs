// SPDX-License-Identifier: Apache-2.0
//! Waiting for hardware, bounded by the CLOCK.
//!
//! # Why this exists
//!
//! Every driver waits for a register: a reset bit to clear, a ready bit to set, a busy line to drop.
//! Before this module each driver wrote that loop itself, and the copies disagreed:
//!
//! - **Some bounded the wait by an iteration count** (`while ... { t += 1; if t > 1_000_000 ...`). A count
//!   is not a duration (CLAUDE.md 26.6): a million register reads is a fraction of a second on one
//!   machine and several seconds on a board reaching its device across a bus.
//! - **The time-bounded ones each re-derived the conversion** from `tsc_ticks_per_10ms`, and disagreed
//!   about the one case that matters most: a machine whose clock the kernel could not calibrate (the
//!   rate reads 0). Some fell back to a poll count; others turned the zero into a one-tick budget and
//!   gave up after a single look.
//!
//! This is the one loop, written once.
//!
//! # The shapes
//!
//! ```ignore
//! use godspeed::driver::wait::{self, Budget};
//!
//! // A single condition: true when the wait is over.
//! if wait::until(ctx, Budget::ms(100), || m.read32(CTRL) & RESET == 0).is_err() {
//!     ctx.log("mydev: the controller never left reset");   // the CALLER says which wait, in its words
//! }
//!
//! // A loop with more than one way out (done, an error bit, or out of time):
//! let mut d = wait::Deadline::start(ctx, Budget::ms(500));
//! loop {
//!     let i = m.read32(INT);
//!     if i & DONE != 0 { break; }
//!     if i & ERR != 0 { return Err("the device reported an error"); }
//!     if d.expired() { return Err("the device never reported done"); }
//! }
//!
//! // A wait measured in seconds, which should not hammer the device: the same, PACED.
//! let mut d = wait::Deadline::paced(ctx, Budget::ms(3_000), Budget::ms(1));
//! loop {
//!     if ready() { break; }
//!     if d.expired() { return Err("the function never reported ready"); }
//!     d.pause();
//! }
//! ```
//!
//! # What it will not do for you
//!
//! **It does not log.** Expiry is returned, never swallowed (`TimedOut` is `#[must_use]` through
//! `Result`), and the caller reports it - because "the controller never left reset" and "the clock
//! never stabilised" are different faults, and only the driver knows which wait this was (26.7).
//!
//! **It sleeps only when asked to.** [`until`] and [`Deadline::start`] poll: a wait measured in
//! microseconds cannot afford a scheduler quantum. A wait measured in seconds should not spend them
//! hammering the device, and [`until_paced`] and [`Deadline::paced`] sleep a PACE between looks.
//!
//! The pace is the wait's, not the caller's, and that is the point of having it here: a caller sleeping
//! inside its own `Deadline` loop (as `xhci` and `sdk/wifi` did) leaves the uncalibrated count below
//! measuring looks it does not know are seconds apart.
//!
//! **On an uncalibrated machine the bound is a count.** There is no other bound to have. A polling wait
//! gets [`UNCALIBRATED_POLLS`] looks, the figure the network drivers had already settled on. A paced
//! wait gets as many looks as its pace fits into its budget, because 200,000 looks with a sleep between
//! each is not a bound anyone meant; and since the kernel's sleep is itself only approximate on such a
//! machine (`sleep_ms` floors to one scheduler quantum), that is a count of pauses, not a duration.
//! Either way the wait still ends, it just does not end on time. [`calibrated`] says which case this
//! machine is in, for a caller that wants to report it.

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

/// How many looks a wait gets when the kernel has no calibrated clock to measure it by.
pub const UNCALIBRATED_POLLS: u32 = 200_000;

/// How long a wait may take. Microsecond resolution, because some hardware gaps are that short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    us: u64,
}

impl Budget {
    pub const fn us(us: u64) -> Self {
        Budget { us }
    }
    pub const fn ms(ms: u64) -> Self {
        Budget { us: ms.saturating_mul(1000) }
    }
    pub const fn as_us(self) -> u64 {
        self.us
    }
}

/// The wait ran out of budget before its condition held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimedOut;

/// Counter ticks in `us` microseconds at `per_10ms` ticks per 10 ms, never zero: a zero budget would
/// expire before the condition was looked at even once.
pub(crate) fn ticks_for(per_10ms: u64, us: u64) -> u64 {
    (per_10ms.saturating_mul(us) / 10_000).max(1)
}

/// Looks an uncalibrated paced wait gets: as many paces as fit in the budget, rounded up, never zero.
pub(crate) fn paced_looks(budget_us: u64, pace_us: u64) -> u32 {
    let pace = pace_us.max(1);
    let looks = budget_us / pace + (budget_us % pace != 0) as u64;
    looks.clamp(1, u32::MAX as u64) as u32
}

/// Microseconds in `ticks` at `per_10ms` ticks per 10 ms. 0 when uncalibrated, which is "unknown".
pub(crate) fn us_for(per_10ms: u64, ticks: u64) -> u64 {
    if per_10ms == 0 { 0 } else { ticks.saturating_mul(10_000) / per_10ms }
}

/// Does this machine have a calibrated clock, so that a [`Budget`] is a real duration?
#[cfg(not(test))]
pub fn calibrated(ctx: &ServiceContext) -> bool {
    ctx.tsc_ticks_per_10ms() != 0
}

/// A running bound. Make it with [`Deadline::start`] (polling) or [`Deadline::paced`] (sleeping), ask
/// [`Deadline::expired`] once per look, and call [`Deadline::pause`] between looks.
#[cfg(not(test))]
pub struct Deadline<'a> {
    ctx: &'a ServiceContext,
    start: u64,
    /// The budget in counter ticks. Unused when uncalibrated.
    ticks: u64,
    /// Ticks per 10 ms, 0 when the kernel could not calibrate.
    per_10ms: u64,
    /// Looks remaining, counted only when uncalibrated.
    polls_left: u32,
    /// Milliseconds [`Deadline::pause`] sleeps; 0 for a polling deadline, whose pause is a spin hint.
    pace_ms: u64,
}

#[cfg(not(test))]
impl<'a> Deadline<'a> {
    /// A polling deadline: [`Deadline::pause`] is only a spin hint. On an uncalibrated machine the
    /// budget becomes [`UNCALIBRATED_POLLS`] looks.
    pub fn start(ctx: &'a ServiceContext, budget: Budget) -> Self {
        let per_10ms = ctx.tsc_ticks_per_10ms();
        Deadline {
            ctx,
            start: ctx.read_tsc(),
            ticks: ticks_for(per_10ms, budget.as_us()),
            per_10ms,
            polls_left: UNCALIBRATED_POLLS,
            pace_ms: 0,
        }
    }

    /// A deadline whose looks are `pace` apart: call [`Deadline::pause`] between them. The pace is in
    /// whole milliseconds, the kernel's sleep resolution, and at least one. On an uncalibrated machine
    /// the budget becomes as many looks as the pace fits into it.
    pub fn paced(ctx: &'a ServiceContext, budget: Budget, pace: Budget) -> Self {
        let pace_ms = (pace.as_us() / 1000).max(1);
        let mut d = Deadline::start(ctx, budget);
        d.pace_ms = pace_ms;
        d.polls_left = paced_looks(budget.as_us(), pace_ms.saturating_mul(1000));
        d
    }

    /// Wait out one pace between looks: a sleep for a paced deadline, a spin hint for a polling one.
    pub fn pause(&self) {
        if self.pace_ms == 0 {
            core::hint::spin_loop();
        } else {
            self.ctx.sleep_ms(self.pace_ms);
        }
    }

    /// Has the budget run out? Call it once per look, AFTER checking the condition, so a condition
    /// already true costs no clock read at all.
    pub fn expired(&mut self) -> bool {
        if self.per_10ms == 0 {
            if self.polls_left == 0 {
                return true;
            }
            self.polls_left -= 1;
            return false;
        }
        // `wrapping_sub` so a counter that wraps mid-wait still measures the interval correctly.
        self.ctx.read_tsc().wrapping_sub(self.start) >= self.ticks
    }

    /// Microseconds since the start, for a caller that reports how long a wait took. 0 when
    /// uncalibrated, where it cannot be known.
    pub fn elapsed_us(&self) -> u64 {
        us_for(self.per_10ms, self.ctx.read_tsc().wrapping_sub(self.start))
    }
}

/// A moment to measure from, KEPT - what a [`Deadline`] cannot be, because it borrows the context it reads
/// the clock through and so cannot outlive the call that made it.
///
/// For a wait that is not a loop: something started now and asked about later, from a different call. A
/// radio's sweep is the first - each channel is listened to for a dwell, and the serve loop asks the radio
/// once per turn whether it is time to tune the next; and the same loop times how long it has gone
/// without reading a joined radio, and how long a request took to serve.
///
/// **On an uncalibrated machine every budget has passed** and nothing has elapsed. There is no duration
/// to compare with, and the alternative - nothing ever passes - turns a dwell into a hang; this way a
/// sweep still runs, only too fast to hear everything. [`calibrated`] says which case this is.
#[derive(Clone, Copy)]
pub struct Since {
    start: u64,
    per_10ms: u64,
}

#[cfg(not(test))]
impl Since {
    /// Now.
    pub fn now(ctx: &ServiceContext) -> Self {
        Since { start: ctx.read_tsc(), per_10ms: ctx.tsc_ticks_per_10ms() }
    }

    /// Has `budget` passed since this moment?
    pub fn passed(&self, ctx: &ServiceContext, budget: Budget) -> bool {
        if self.per_10ms == 0 {
            return true;
        }
        ctx.read_tsc().wrapping_sub(self.start) >= ticks_for(self.per_10ms, budget.as_us())
    }

    /// Microseconds since this moment; 0 when uncalibrated, where it cannot be known.
    pub fn elapsed_us(&self, ctx: &ServiceContext) -> u64 {
        us_for(self.per_10ms, ctx.read_tsc().wrapping_sub(self.start))
    }
}

/// Wait until `cond` returns true or `budget` runs out. `Ok` carries the microseconds it took (0 if
/// the machine is uncalibrated or the condition already held).
#[cfg(not(test))]
pub fn until(ctx: &ServiceContext, budget: Budget, mut cond: impl FnMut() -> bool) -> Result<u64, TimedOut> {
    if cond() {
        return Ok(0);
    }
    let mut d = Deadline::start(ctx, budget);
    loop {
        if cond() {
            return Ok(d.elapsed_us());
        }
        if d.expired() {
            return Err(TimedOut);
        }
        core::hint::spin_loop();
    }
}

/// [`until`], sleeping `pace` between looks: for a wait measured in seconds, where a look every
/// moment would only hammer the device. See [`Deadline::paced`].
#[cfg(not(test))]
pub fn until_paced(
    ctx: &ServiceContext,
    budget: Budget,
    pace: Budget,
    mut cond: impl FnMut() -> bool,
) -> Result<u64, TimedOut> {
    if cond() {
        return Ok(0);
    }
    let mut d = Deadline::paced(ctx, budget, pace);
    loop {
        d.pause();
        if cond() {
            return Ok(d.elapsed_us());
        }
        if d.expired() {
            return Err(TimedOut);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_units() {
        assert_eq!(Budget::ms(3).as_us(), 3_000);
        assert_eq!(Budget::us(7).as_us(), 7);
        assert_eq!(Budget::ms(u64::MAX).as_us(), u64::MAX);
    }

    #[test]
    fn ticks_are_never_zero() {
        assert_eq!(ticks_for(0, 1_000), 1);
        assert_eq!(ticks_for(540_000, 0), 1); // 54 MHz, no budget
        assert_eq!(ticks_for(5_000, 1), 1); // 0.5 MHz: one microsecond is under a tick, and rounds up to one
    }

    #[test]
    fn ticks_follow_the_calibration() {
        // The Pi 4's generic timer: 54 MHz, 540_000 ticks per 10 ms.
        assert_eq!(ticks_for(540_000, 10_000), 540_000);
        assert_eq!(ticks_for(540_000, 500_000), 27_000_000);
        // A ~2 GHz x86 TSC.
        assert_eq!(ticks_for(20_000_000, 1_000), 2_000_000);
        assert_eq!(ticks_for(u64::MAX, u64::MAX), u64::MAX / 10_000);
    }

    #[test]
    fn paced_looks_fit_the_budget() {
        assert_eq!(paced_looks(3_000_000, 1_000), 3_000); // 3 s at 1 ms
        assert_eq!(paced_looks(1_000_000, 10_000), 100); // 1 s at 10 ms - the mmc core's CMD5 retry
        assert_eq!(paced_looks(1_500, 1_000), 2); // a partial pace still gets its look
        assert_eq!(paced_looks(0, 1_000), 1); // never zero: the condition is always looked at
        assert_eq!(paced_looks(5, 0), 5); // a zero pace is read as one microsecond, not a division by zero
        assert_eq!(paced_looks(u64::MAX, 1), u32::MAX);
    }

    #[test]
    fn elapsed_round_trips() {
        assert_eq!(us_for(540_000, ticks_for(540_000, 123_000)), 123_000);
        assert_eq!(us_for(0, 999), 0);
    }
}
