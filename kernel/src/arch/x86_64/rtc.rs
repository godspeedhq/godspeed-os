// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! MC146818 CMOS real-time clock (§12, arch hardware boundary).
//!
//! Read-only wall-clock access. The RTC is a legacy device on the I/O ports
//! 0x70 (index) / 0x71 (data); port I/O is ring-0, so - like the PIT, PIC, and
//! serial UART - it lives in the arch layer rather than a userspace driver
//! (there is no I/O-port capability in v1, and the clock is a tiny read-only
//! device). Userspace reads it via `InspectKernel` query 11, ungated, because
//! the time of day is task-neutral hardware info (like the TSC clock).

use core::sync::atomic::{AtomicU64, Ordering};

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

/// The packed wall-clock datetime captured once at boot (see `read_datetime` for the layout).
/// 0 until `capture_boot_time` runs. `uptime` reads it via InspectKernel query 12 and subtracts
/// it from the current time - a wall-clock delta, portable across APIC timer modes (a tick
/// counter's rate is not).
static BOOT_DATETIME: AtomicU64 = AtomicU64::new(0);

/// Record the current RTC time as the system's boot time. Called once early in `kernel_main`.
/// Idempotent - only the first capture sticks (a 0 reading, i.e. no RTC, leaves uptime at 0).
pub fn capture_boot_time() {
    let now = read_datetime();
    let _ = BOOT_DATETIME.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
}

/// The packed boot datetime (0 if not yet captured). Exposed via InspectKernel query 12.
pub fn boot_datetime() -> u64 {
    BOOT_DATETIME.load(Ordering::Relaxed)
}

/// Read one CMOS register.
fn cmos_read(reg: u8) -> u8 {
    // SAFETY: 0x70/0x71 are the standard CMOS index/data ports. Writing a register
    // number (0x00..0x3F, bit 7 clear) to 0x70 selects it; reading 0x71 returns its
    // value. Pure port I/O with no memory effects. The two asm blocks are not `pure`,
    // so the compiler preserves their order (index write before data read).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") CMOS_INDEX,
            in("al") reg,
            options(nostack, nomem, preserves_flags),
        );
        let val: u8;
        core::arch::asm!(
            "in al, dx",
            in("dx") CMOS_DATA,
            out("al") val,
            options(nostack, nomem, preserves_flags),
        );
        val
    }
}

/// Status register A bit 7: an RTC update is in progress (values are changing).
#[inline]
fn update_in_progress() -> bool {
    cmos_read(0x0A) & 0x80 != 0
}

#[inline]
fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// Read the RTC wall-clock, retrying for a plausible result. A glitched year/century register (a rare
/// CMOS misread) would otherwise stamp a wild datetime; because the boot stamp and each task's spawn
/// stamp are captured ONCE and stick, a single bad read makes every uptime delta from it enormous - the
/// ~1.7e9-second values seen in `observe`/`uptime`. `read_datetime_raw`'s double-read already rejects a
/// tick landing mid-read; this outer guard additionally rejects a decoded year outside a plausible
/// window and re-reads (bounded). If every attempt is implausible (a truly stuck RTC) it returns the
/// last read - the per-service uptime is still capped at the system uptime downstream (scheduler).
pub fn read_datetime() -> u64 {
    let mut dt = read_datetime_raw();
    let mut tries = 0;
    while !year_plausible(dt) && tries < 8 {
        dt = read_datetime_raw();
        tries += 1;
    }
    dt
}

/// A DEGLITCHED "now" in epoch seconds: monotonic + forward-jump-bounded, for time-DELTA uses (per-service
/// uptime). `read_datetime` already rejects an implausible YEAR and a mid-update read, but a CMOS misread
/// landing on an in-range year (e.g. 2039) still slips through and, because uptime = now - stamp, surfaces
/// as a wild value (the "4987d" bug). This high-water-mark drops any read that goes BACKWARDS or jumps
/// FORWARD by more than a day - always a glitch for a delta sampled every observe frame - and holds the last
/// good value instead. A single kernel-owned clock cache (like MONOTONIC_TICKS or the boot stamp), not
/// service state. `date` keeps the raw read (an on-demand display tolerates a rare one-off).
static LAST_NOW_EPOCH: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);
pub fn now_epoch_monotonic() -> i64 {
    use core::sync::atomic::Ordering;
    let last     = LAST_NOW_EPOCH.load(Ordering::Relaxed);
    // The pure decision lives in `crate::clock` (hardware-free, host-unit-tested); this wrapper just
    // supplies the live RTC read and the high-water-mark store.
    let accepted = crate::clock::deglitch_epoch(epoch_secs(read_datetime()), last);
    LAST_NOW_EPOCH.store(accepted, Ordering::Relaxed);
    accepted
}

/// A decoded year inside the window this build can sanely run in; a read outside it is a glitch.
fn year_plausible(packed: u64) -> bool {
    (2020..=2100).contains(&((packed >> 26) & 0xFFF))
}

/// Read the RTC and return the wall-clock date/time packed into a `u64`:
///
/// ```text
///   bits  0..6   second (0–59)
///   bits  6..12  minute (0–59)
///   bits 12..17  hour   (0–23)
///   bits 17..22  day    (1–31)
///   bits 22..26  month  (1–12)
///   bits 26..38  year   (full, e.g. 2026)
/// ```
///
/// Robust against an RTC tick landing mid-read: it reads every field, reads them
/// again, and repeats until two consecutive reads agree (the standard algorithm -
/// no need to disable interrupts). Decodes BCD and 12-hour mode per status
/// register B, so the returned fields are always binary and 24-hour.
fn read_datetime_raw() -> u64 {
    while update_in_progress() {}
    let (mut sec, mut min, mut hour) = (cmos_read(0), cmos_read(2), cmos_read(4));
    let (mut day, mut month, mut year, mut century) =
        (cmos_read(7), cmos_read(8), cmos_read(9), cmos_read(0x32));
    loop {
        let prev = (sec, min, hour, day, month, year, century);
        while update_in_progress() {}
        sec = cmos_read(0);
        min = cmos_read(2);
        hour = cmos_read(4);
        day = cmos_read(7);
        month = cmos_read(8);
        year = cmos_read(9);
        century = cmos_read(0x32);
        if prev == (sec, min, hour, day, month, year, century) {
            break;
        }
    }

    let regb = cmos_read(0x0B);
    let is_binary = regb & 0x04 != 0; // DM: 1 = binary, 0 = BCD
    let is_24h = regb & 0x02 != 0; // 1 = 24-hour, 0 = 12-hour

    // The 12-hour PM flag is bit 7 of the raw hour byte; preserve it across BCD
    // decode, which only touches the low 7 bits.
    let pm = hour & 0x80 != 0;
    if is_binary {
        hour &= 0x7F;
    } else {
        sec = bcd_to_bin(sec);
        min = bcd_to_bin(min);
        hour = bcd_to_bin(hour & 0x7F);
        day = bcd_to_bin(day);
        month = bcd_to_bin(month);
        year = bcd_to_bin(year);
        century = bcd_to_bin(century);
    }
    if !is_24h {
        if pm {
            hour = (hour % 12) + 12; // 1–11 PM → 13–23, 12 PM → 12
        } else if hour == 12 {
            hour = 0; // 12 AM → 00
        }
    }

    // Full year. The century register (0x32) is present on PCs and QEMU; if it
    // reads an implausible value (an absent register can read 0 or 0xFF), assume
    // the 2000s.
    let century = if (19..=21).contains(&century) { century as u64 } else { 20 };
    let full_year = century * 100 + (year as u64 % 100);

    (sec as u64 & 0x3F)
        | ((min as u64 & 0x3F) << 6)
        | ((hour as u64 & 0x1F) << 12)
        | ((day as u64 & 0x1F) << 17)
        | ((month as u64 & 0x0F) << 22)
        | ((full_year & 0xFFF) << 26)
}

/// Seconds since the Unix epoch for a `read_datetime`/`boot_datetime`-packed value. The pure civil-date math
/// (leap-year correct, Howard Hinnant's days_from_civil - matches the SDK's `Datetime::epoch_secs`) lives in
/// the host-testable `crate::clock` and is re-exported here; this arch module supplies only the live RTC
/// reads. The absolute epoch cancels in a difference, so this is the clock for per-service uptime: a real
/// wall-clock (unlike the BSP-idle-gated MONOTONIC_TICKS) and single-sourced (unlike the cross-core-skewed TSC).
pub use crate::clock::epoch_secs;
