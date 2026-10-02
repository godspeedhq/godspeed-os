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
//! # The two shapes
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
//! ```
//!
//! # What it will not do for you
//!
//! **It does not log.** Expiry is returned, never swallowed (`TimedOut` is `#[must_use]` through
//! `Result`), and the caller reports it - because "the controller never left reset" and "the clock
//! never stabilised" are different faults, and only the driver knows which wait this was (26.7).
//!
//! **It does not sleep.** It polls. A wait measured in microseconds cannot afford a scheduler quantum,
//! and one measured in seconds should be pacing itself with `ctx.sleep_ms` between looks, which a
//! caller can do inside its own `Deadline` loop.
//!
//! **On an uncalibrated machine the bound is a count.** There is no other bound to have. It is
//! [`UNCALIBRATED_POLLS`] looks, the figure the network drivers had already settled on; the wait still
//! ends, it just does not end on time. [`calibrated`] says which case this machine is in, for a caller
//! that wants to report it.

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

/// Microseconds in `ticks` at `per_10ms` ticks per 10 ms. 0 when uncalibrated, which is "unknown".
pub(crate) fn us_for(per_10ms: u64, ticks: u64) -> u64 {
    if per_10ms == 0 { 0 } else { ticks.saturating_mul(10_000) / per_10ms }
}

/// Does this machine have a calibrated clock, so that a [`Budget`] is a real duration?
#[cfg(not(test))]
pub fn calibrated(ctx: &ServiceContext) -> bool {
    ctx.tsc_ticks_per_10ms() != 0
}

/// A running bound. Start it, then ask [`Deadline::expired`] once per look.
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
}

#[cfg(not(test))]
impl<'a> Deadline<'a> {
    pub fn start(ctx: &'a ServiceContext, budget: Budget) -> Self {
        let per_10ms = ctx.tsc_ticks_per_10ms();
        Deadline {
            ctx,
            start: ctx.read_tsc(),
            ticks: ticks_for(per_10ms, budget.as_us()),
            per_10ms,
            polls_left: UNCALIBRATED_POLLS,
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
    fn elapsed_round_trips() {
        assert_eq!(us_for(540_000, ticks_for(540_000, 123_000)), 123_000);
        assert_eq!(us_for(0, 999), 0);
    }
}
