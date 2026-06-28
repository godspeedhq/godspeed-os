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

/// Seconds since the Unix epoch for a `read_datetime`/`boot_datetime`-packed value (leap-year correct,
/// Howard Hinnant's days_from_civil - matches the SDK's `Datetime::epoch_secs`). Pure bit-unpack + civil
/// date math, so it lives here (host-testable) and is re-exported by `arch/x86_64/rtc.rs`. Packed layout
/// (LSB first): sec[6] min[6] hour[5] day[5] month[4] year[12]. Pinned below against an independent naive
/// reference over a multi-century sweep, so a future "simplification" can't silently break dates + uptime.
pub fn epoch_secs(packed: u64) -> i64 {
    let sec   = (packed & 0x3F) as i64;
    let min   = ((packed >> 6) & 0x3F) as i64;
    let hour  = ((packed >> 12) & 0x1F) as i64;
    let day   = ((packed >> 17) & 0x1F) as i64;
    let month = ((packed >> 22) & 0x0F) as i64;
    let mut y = ((packed >> 26) & 0xFFF) as i64;
    y -= (month <= 2) as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86_400 + hour * 3_600 + min * 60 + sec
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

#[cfg(test)]
mod epoch_tests {
    use super::epoch_secs;

    // Pack fields into the RTC's `read_datetime` layout (LSB first): sec[6] min[6] hour[5] day[5] month[4] year[12].
    fn pack(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> u64 {
        (s as u64 & 0x3F)
            | ((mi as u64 & 0x3F) << 6)
            | ((h  as u64 & 0x1F) << 12)
            | ((d  as u64 & 0x1F) << 17)
            | ((mo as u64 & 0x0F) << 22)
            | ((y  as u64 & 0xFFF) << 26)
    }

    fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }
    fn days_in_month(y: i64, m: i64) -> i64 {
        match m {
            1 => 31, 2 => if is_leap(y) { 29 } else { 28 }, 3 => 31, 4 => 30, 5 => 31, 6 => 30,
            7 => 31, 8 => 31, 9 => 30, 10 => 31, 11 => 30, 12 => 31, _ => 0,
        }
    }

    /// A deliberately naive, obviously-correct reference: count days from 1970 by iterating years + months.
    /// Slow, but it cannot share a bug with Hinnant's closed form - which is exactly what makes the cross-check valid.
    fn reference_epoch(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        let mut days: i64 = 0;
        for yy in 1970..y { days += if is_leap(yy) { 366 } else { 365 }; }
        for mm in 1..mo  { days += days_in_month(y, mm); }
        days += d - 1;
        days * 86_400 + h * 3_600 + mi * 60 + s
    }

    #[test]
    fn reference_matches_known_unix_anchors() {
        // Validate the REFERENCE itself against well-known Unix epochs first, so the cross-check below is trustworthy.
        assert_eq!(reference_epoch(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(reference_epoch(2000, 1, 1, 0, 0, 0), 946_684_800);
        assert_eq!(reference_epoch(2038, 1, 19, 3, 14, 8), 2_147_483_648); // Y2038 boundary (2^31)
        assert_eq!(reference_epoch(2000, 2, 29, 0, 0, 0), 951_782_400);    // leap day
    }

    #[test]
    fn epoch_secs_matches_known_unix_anchors() {
        assert_eq!(epoch_secs(pack(1970, 1, 1, 0, 0, 0)), 0);
        assert_eq!(epoch_secs(pack(2000, 1, 1, 0, 0, 0)), 946_684_800);
        assert_eq!(epoch_secs(pack(2038, 1, 19, 3, 14, 8)), 2_147_483_648);
    }

    #[test]
    fn epoch_secs_matches_reference_over_a_multi_century_sweep() {
        // Cross-check Hinnant (epoch_secs) vs the naive reference for every month of 1971..=2100 - covering
        // ordinary leap years (div-4), the century NON-leap (2100), and the leap-400 (2000). Last-day-of-month
        // catches the Feb 28/29 boundary.
        for y in 1971..=2100i64 {
            for mo in 1..=12i64 {
                let last = days_in_month(y, mo);
                for &d in &[1i64, 15, 28, last] {
                    for &(h, mi, s) in &[(0i64, 0i64, 0i64), (23, 59, 59), (12, 30, 15)] {
                        assert_eq!(
                            epoch_secs(pack(y, mo, d, h, mi, s)),
                            reference_epoch(y, mo, d, h, mi, s),
                            "epoch_secs mismatch at {}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s);
                    }
                }
            }
        }
    }

    #[test]
    fn leap_rules_are_exact_at_the_feb_boundary() {
        // Feb->Mar is exactly one day across every leap rule.
        let span = |y, last_feb| epoch_secs(pack(y, 3, 1, 0, 0, 0)) - epoch_secs(pack(y, 2, last_feb, 0, 0, 0));
        assert_eq!(span(2024, 29), 86_400); // div-4 leap: Feb 29 exists
        assert_eq!(span(2000, 29), 86_400); // leap-400: Feb 29 exists
        assert_eq!(span(2100, 28), 86_400); // century non-leap: Feb has 28
        assert_eq!(span(2400, 29), 86_400); // leap-400: Feb 29 exists
    }
}
