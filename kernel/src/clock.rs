// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Pure clock-deglitch logic - no hardware dependencies, so it is host-unit-testable. Declared in BOTH
//! `lib.rs` (the host test target) and `main.rs` (the bin), the same shared-pure-module pattern as
//! `elf_flags`. The arch-side RTC reader (`arch/x86_64/rtc.rs::now_epoch_monotonic`) supplies the live
//! reads + the high-water-mark store and defers the decision to `deglitch_epoch` here.

/// The pure deglitch decision behind `now_epoch_monotonic`. Given a freshly read epoch `raw` and the last
/// accepted value `last`, return the value to adopt:
///   - the first read (`last == 0`) is taken as-is;
///   - a read going BACKWARDS, or jumping FORWARD by more than a day, is rejected and `last` is held;
///   - anything else is a normal advance and is accepted.
///
/// The >1-day bound is the "4987d" guard: a per-frame uptime delta never legitimately jumps a day, so a
/// CMOS misread landing on an in-range future year (which `rtc::read_datetime`'s year window passes) is
/// dropped here rather than inflating per-service uptime to thousands of days. Pinned by the tests below.
pub fn deglitch_epoch(raw: i64, last: i64) -> i64 {
    if last == 0 { return raw; }
    if raw < last || raw > last + 86_400 { return last; }
    raw
}

#[cfg(test)]
mod tests {
    use super::deglitch_epoch;
    const DAY: i64 = 86_400;
    const T: i64 = 1_800_000_000; // a plausible ~2027 epoch (seconds)

    #[test]
    fn first_read_is_taken_as_is() {
        assert_eq!(deglitch_epoch(T, 0), T);
    }

    #[test]
    fn normal_advance_is_accepted() {
        assert_eq!(deglitch_epoch(T + 5, T), T + 5);
    }

    #[test]
    fn backwards_step_is_held() {
        // A clock that appears to go backwards is a glitch; hold the last good value, never regress.
        assert_eq!(deglitch_epoch(T - 10, T), T);
    }

    #[test]
    fn forward_up_to_a_day_is_accepted() {
        assert_eq!(deglitch_epoch(T + DAY - 1, T), T + DAY - 1);
        assert_eq!(deglitch_epoch(T + DAY, T), T + DAY); // boundary: exactly a day is still allowed
    }

    #[test]
    fn the_4987d_glitch_is_held() {
        // A CMOS misread landing ~13 years ahead - an in-range year, so rtc's year-guard passes it. This
        // is the exact "4987d" bug; the >1-day bound must drop it.
        assert_eq!(deglitch_epoch(T + 13 * 365 * DAY, T), T);
        // and a day-plus-one is already rejected: the boundary is tight, not loose.
        assert_eq!(deglitch_epoch(T + DAY + 1, T), T);
    }
}
