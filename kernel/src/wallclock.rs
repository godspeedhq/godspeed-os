// SPDX-License-Identifier: GPL-2.0-only
//! The wall clock's PROVENANCE and its FLOOR - the stateful half of timekeeping.
//!
//! `clock.rs` is pure date math (host-testable, shared with the lib target). This module is the part that
//! holds state and talks to the arch layer, so it is bin-only.
//!
//! **Provenance.** A wall-clock reading can come from a local hardware RTC, from the network (SNTP), or
//! from nowhere at all. Which one it was is part of the answer: §26.4 rejects "automatic fallback
//! behavior" precisely when the fallback is invisible, so the chain is only mechanism while its choice is
//! reportable. `date` prints the source alongside the time.
//!
//! **The floor.** A persisted "the machine was demonstrably running at least this late" bound, seeded from
//! disk by the operator's agent at startup. It is deliberately NOT displayed as the time, and no estimate
//! is ever derived from it: a machine powered off for six months would otherwise show a six-month-old
//! timestamp that looks exactly like a measured one - a fabricated fact wearing the costume of a measured
//! one (§26.7, invariant 12). A bound is TRUE ("at least this late"); an estimate is a guess. What the
//! floor is for is REFUSING a time that cannot be right: a dead RTC reading 2000, or a stale/hostile
//! network reply from before we last ran.

use core::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use crate::clock::{epoch_secs, CLOCK_MIN_PLAUSIBLE, CLOCK_MAX_PLAUSIBLE};

pub const CLOCK_SRC_UNSET: u8 = 0;
pub const CLOCK_SRC_RTC:   u8 = 1;
pub const CLOCK_SRC_NTP:   u8 = 2;

/// Set once the network has set the clock this boot; otherwise the source is deduced from whether the
/// arch's hardware clock reads a plausible date.
static WALL_FROM_NETWORK: AtomicU8 = AtomicU8::new(0);
/// Monotonic seconds at the moment of that set, so `date` can say how fresh the sync is (-1 = never).
static WALL_SET_AT_MONO: AtomicI64 = AtomicI64::new(-1);
/// The persisted lower bound (0 = none known).
static CLOCK_FLOOR: AtomicI64 = AtomicI64::new(0);

/// Where the current wall-clock reading comes from. NTP if the network set it this boot; otherwise RTC if
/// the hardware clock reads a plausible date; otherwise nothing is known and `date` says so.
pub fn source() -> u8 {
    if WALL_FROM_NETWORK.load(Ordering::Relaxed) != 0 { return CLOCK_SRC_NTP; }
    let e = epoch_secs(crate::arch::imp::rtc::read_datetime());
    if (CLOCK_MIN_PLAUSIBLE..=CLOCK_MAX_PLAUSIBLE).contains(&e) { CLOCK_SRC_RTC } else { CLOCK_SRC_UNSET }
}

/// Seconds since the network last set the clock, or -1 if it never did this boot.
pub fn synced_secs_ago() -> i64 {
    let at = WALL_SET_AT_MONO.load(Ordering::Relaxed);
    if at < 0 { return -1; }
    (crate::arch::imp::rtc::now_epoch_monotonic() - at).max(0)
}

pub fn floor() -> i64 { CLOCK_FLOOR.load(Ordering::Relaxed) }

/// Raise the persisted floor. Monotonic by construction - a floor only ever moves FORWARD, so a stale disk
/// value cannot lower a better bound, and a caller cannot un-bound a clock it already bounded.
pub fn set_floor(epoch: i64) -> bool {
    if !(CLOCK_MIN_PLAUSIBLE..=CLOCK_MAX_PLAUSIBLE).contains(&epoch) { return false; }
    let cur = CLOCK_FLOOR.load(Ordering::Relaxed);
    if epoch <= cur { return true; }                       // already at least this late - nothing to do
    CLOCK_FLOOR.store(epoch, Ordering::Relaxed);
    true
}

/// Set the wall clock from a network time source. Refused - loudly - if the value is outside the plausible
/// window or EARLIER than the floor we already know we ran at. A correction in either direction above the
/// floor is fine (a clock running fast is legitimately pulled back); what cannot be right is a time from
/// before the last moment the machine is known to have been alive.
pub fn set_wall_clock(epoch: i64) -> bool {
    if !(CLOCK_MIN_PLAUSIBLE..=CLOCK_MAX_PLAUSIBLE).contains(&epoch) {
        crate::kprintln!("clock: REFUSED epoch {} - outside the plausible window", epoch);
        return false;
    }
    let f = CLOCK_FLOOR.load(Ordering::Relaxed);
    if f != 0 && epoch < f {
        crate::kprintln!("clock: REFUSED epoch {} - earlier than the floor {} (we ran at least that late)",
                         epoch, f);
        return false;
    }
    let old = epoch_secs(crate::arch::imp::rtc::read_datetime());
    crate::arch::imp::rtc::set_wall_clock(epoch);
    WALL_FROM_NETWORK.store(1, Ordering::Relaxed);
    WALL_SET_AT_MONO.store(crate::arch::imp::rtc::now_epoch_monotonic(), Ordering::Relaxed);
    set_floor(epoch);          // we are demonstrably running now, at this time
    crate::kprintln!("clock: wall clock set to epoch {} from the network (was {})", epoch, old);
    true
}
