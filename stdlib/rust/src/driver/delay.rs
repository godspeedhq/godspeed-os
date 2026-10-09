// SPDX-License-Identifier: Apache-2.0
//! Holding still for a set time, waiting for nothing.
//!
//! # Why this exists
//!
//! Some hardware needs a gap that no register reports: let a reset bit land before the next write,
//! give a port the recovery time a specification demands before it is addressed. Nothing can be read to
//! say the gap is over, so this is not a [`wait`](super::wait) - there is no condition, only a
//! duration. Drivers wrote it by hand, and the copies disagreed about the one case that matters, a
//! machine whose clock the kernel could not calibrate:
//!
//! - `genet`'s `delay_us` yielded once, which returns at once when nothing else is runnable;
//! - `xhci`'s two holds built their length from `duration_cycles`, which floors to one counter tick
//!   there, so they held for nothing at all.
//!
//! Both are the same mistake in the direction that matters. A hold is a MINIMUM: the device needs at
//! least this long, and the safe error is too long, never too short.
//!
//! # What it does
//!
//! ```ignore
//! use godspeed::driver::delay;
//! use godspeed::driver::wait::Budget;
//!
//! m.write32(CMD, SW_RESET);
//! delay::hold(ctx, Budget::us(10)); // the reset bit needs time to land; nothing says when it has
//! m.write32(CMD, 0);
//! ```
//!
//! **On a calibrated machine [`hold`] spins** for the duration, measured by the counter. It does not
//! sleep: the kernel's sleep floors at a scheduler quantum, nominally 10 ms (CLAUDE.md 9.1), which would
//! turn a 10 us hold into a thousand times that. **[`hold_parked`] sleeps first and spins out the
//! rest**, for holds of tens of milliseconds where the core matters more than the microsecond: it can
//! run up to a quantum long, which a minimum allows, and is never short. It has two callers (`ehci`, and
//! `wifi-usb`'s rtl8188 backend), and it is in on probation: `docs/driver-library.md` 1l says why.
//!
//! **On an uncalibrated one both sleep whole quanta**, as many as the duration needs at the nominal
//! 10 ms: the quantum is measured by the kernel's tick rather than the counter nobody could calibrate,
//! so it is the one duration left, and it errs long. [`super::wait::calibrated`] says which case this
//! machine is in.
//!
//! **It does not log**, for the reason [`wait`](super::wait) does not: it cannot fail.

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

#[cfg(not(test))]
use super::wait::Budget;

/// The scheduler quantum the constitution fixes (CLAUDE.md 9.1), in microseconds: the length an
/// uncalibrated hold counts its sleeps in.
const NOMINAL_QUANTUM_US: u64 = 10_000;

/// Sleeps an uncalibrated hold of `us` microseconds takes: enough nominal quanta to cover it, at least
/// one, so a hold is never skipped.
pub(crate) fn quanta_for(us: u64) -> u64 {
    (us / NOMINAL_QUANTUM_US + (us % NOMINAL_QUANTUM_US != 0) as u64).max(1)
}

/// Hold for at least `d`. Spins on a calibrated machine; sleeps whole scheduler quanta on one that is
/// not. See the module documentation.
#[cfg(not(test))]
pub fn hold(ctx: &ServiceContext, d: Budget) {
    let per_10ms = ctx.tsc_ticks_per_10ms();
    if per_10ms == 0 {
        for _ in 0..quanta_for(d.as_us()) {
            ctx.sleep_ms(1); // floors to one quantum on this machine, which is the point
        }
        return;
    }
    let ticks = super::wait::ticks_for(per_10ms, d.as_us());
    let start = ctx.read_tsc();
    spin_until(ctx, start, ticks);
}

/// Hold for at least `d`, PARKED: sleep through it, then spin out whatever the sleep left. For holds of
/// tens of milliseconds - a USB reset, a debounce - where the core matters more than waking on the
/// microsecond.
///
/// A [`hold`] that long spins the whole time, and a driver that holds often pays for it: `ehci`, which
/// holds 50 and 100 ms on every turn of its rescan loop, read 100% of a core in `observe` with nothing
/// plugged in until it learned to park. The sleep can wake late by up to a quantum, which a MINIMUM
/// allows; the spin covers a sleep that woke early, so the hold is never short on a calibrated machine.
/// On an uncalibrated one it is [`hold`]'s whole quanta.
#[cfg(not(test))]
pub fn hold_parked(ctx: &ServiceContext, d: Budget) {
    let per_10ms = ctx.tsc_ticks_per_10ms();
    if per_10ms == 0 {
        return hold(ctx, d);
    }
    let ticks = super::wait::ticks_for(per_10ms, d.as_us());
    let start = ctx.read_tsc();
    ctx.sleep(ticks);
    spin_until(ctx, start, ticks);
}

/// Spin until `ticks` have passed since `start`. `wrapping_sub` so a counter that wraps mid-hold still
/// measures the interval correctly.
#[cfg(not(test))]
fn spin_until(ctx: &ServiceContext, start: u64, ticks: u64) {
    while ctx.read_tsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncalibrated_hold_is_never_skipped() {
        assert_eq!(quanta_for(0), 1);
        assert_eq!(quanta_for(10), 1); // genet's 10 us
        assert_eq!(quanta_for(2_000), 1); // xhci's settle after HCRST
    }

    #[test]
    fn an_uncalibrated_hold_errs_long() {
        assert_eq!(quanta_for(10_000), 1);
        assert_eq!(quanta_for(10_001), 2);
        assert_eq!(quanta_for(55_000), 6); // xhci's reset-recovery hold
        assert_eq!(quanta_for(u64::MAX), u64::MAX / NOMINAL_QUANTUM_US + 1);
    }
}
