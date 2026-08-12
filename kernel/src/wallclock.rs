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

// `AtomicI64` comes from `portable_atomic`, NOT `core` - 32-bit RISC-V (RV32A) has no 64-bit atomic,
// so `core::sync::atomic::AtomicI64` does not exist there and this file would not compile. That is the
// word-size portability rule in `arch/CLAUDE.md`, and the RV32 target exists precisely to catch its
// violation, which it did. `AtomicU8` is fine from `core` - every ISA has an 8-bit atomic.
use core::sync::atomic::{AtomicU8, Ordering};
use portable_atomic::AtomicI64;
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

/// How far past a KNOWN wall clock a floor may be pushed. A floor is "we were running at least this late",
/// so it can never legitimately be ahead of the current time by more than clock skew. Without this bound a
/// floor holder could set it to 2099 and permanently refuse every later clock set - a one-way brick, since
/// the floor never lowers (§26.6: a subsystem must have a recovery story, not just a limit).
const FLOOR_AHEAD_MAX: i64 = 86_400;

/// Raise the persisted floor. Monotonic - the floor only ever moves FORWARD, so a stale disk value cannot
/// lower a better bound. `fetch_max` rather than load-then-store: both `net-stack` and the `shell` can hold
/// this authority on an SMP board, and a read-modify-write pair would let a slower core's smaller value
/// land last and move the floor BACKWARDS, contradicting the invariant this function exists to keep.
pub fn set_floor(epoch: i64) -> bool {
    if !(CLOCK_MIN_PLAUSIBLE..=CLOCK_MAX_PLAUSIBLE).contains(&epoch) { return false; }
    // If we know what time it is, refuse a floor implausibly far ahead of it.
    let now = epoch_secs(crate::arch::imp::rtc::read_datetime());
    if (CLOCK_MIN_PLAUSIBLE..=CLOCK_MAX_PLAUSIBLE).contains(&now) && epoch > now + FLOOR_AHEAD_MAX {
        crate::kprintln!("clock: REFUSED floor {} - more than a day ahead of the clock ({})", epoch, now);
        return false;
    }
    CLOCK_FLOOR.fetch_max(epoch, Ordering::Relaxed);
    true
}

/// Clear the floor. The escape hatch for a floor that was recorded wrongly (a bad disk value, a clock that
/// was set far into the future before being corrected): without it the only recovery is deleting the
/// on-disk record AND rebooting. Requires the stronger clock-setting right, not the floor-raising one.
pub fn clear_floor() -> bool {
    let old = CLOCK_FLOOR.swap(0, Ordering::Relaxed);
    crate::kprintln!("clock: floor cleared (was {})", old);
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
    // The arch reports whether it actually adopted the value. On a board whose hardware RTC is the
    // authority the hook is a no-op, and recording "ntp" there would make `date` attribute a CMOS reading
    // to the network - a provenance lie in the very mechanism built to prevent provenance lies.
    if !crate::arch::imp::rtc::set_wall_clock(epoch) {
        crate::kprintln!("clock: epoch {} not adopted - this board's hardware clock is the authority", epoch);
        return false;
    }
    WALL_FROM_NETWORK.store(1, Ordering::Relaxed);
    WALL_SET_AT_MONO.store(crate::arch::imp::rtc::now_epoch_monotonic(), Ordering::Relaxed);
    set_floor(epoch);          // we are demonstrably running now, at this time
    crate::kprintln!("clock: wall clock set to epoch {} from the network (was {})", epoch, old);
    true
}
