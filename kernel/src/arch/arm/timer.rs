// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7-A generic timer, cross-checked against the BCM2835 System Timer.
//!
//! ARM hands us something x86 does not: **`CNTFRQ` reports the timer frequency architecturally**, so
//! there is nothing to calibrate. The entire PIT-calibration apparatus the x86 side needs - and the
//! ~1 second quantum bug that came with it on the T630, where an uncalibrated APIC period was 100x
//! the intended 10 ms - simply does not arise here. You read the frequency and you are done.
//!
//! Except that trusting a register because the architecture says to is exactly the assumption worth
//! checking. `CNTFRQ` is **not** discovered by the hardware; it is a plain read/write register that
//! *firmware is supposed to program*. Firmware that forgets leaves it 0 or wrong, and every duration
//! computed from it is then silently wrong - the kind of failure that shows up much later as
//! mysterious timing bugs.
//!
//! So this module does not take `CNTFRQ` on faith. The Pi carries a **second, independent clock**:
//! the BCM2835 System Timer, a free-running 1 MHz counter in the peripheral block, whose rate is
//! fixed by hardware rather than programmed. Measuring one against the other turns "the register says
//! 19.2 MHz" into "two independent clocks agree on how long a second is". Cross-check, do not assume.
//!
//! **That cross-check immediately caught a real bug, and the Raspberry Pi 2 is the counter-example to
//! this module's own opening paragraph.** On the BCM2836 the generic timer is fed through a *core
//! timer prescaler* (`0x4000_0008`) at `source * prescaler / 2^31`. Firmware programs it to
//! `0x06AAAAAB`, which divides the 19.2 MHz crystal to **exactly 1 MHz** - and then leaves `CNTFRQ`
//! advertising the *undivided* 19.2 MHz. So `CNTFRQ` overstates the counter by **19.2x** on this
//! board. Trusting it would have made every delay and every scheduler quantum wrong by that factor,
//! and the symptom would have appeared far from the cause. QEMU cannot reproduce this: it does not
//! model the prescaler at all (both registers read 0) and its `CNTFRQ` is truthful.
//!
//! Hence `timer_hz()` returns the **measured** rate, never `CNTFRQ`. ARM was supposed to make
//! calibration unnecessary; on this board it is necessary anyway, just for a different reason than on
//! x86 - not because the frequency is undiscoverable, but because the register that reports it lies.
//!
//! This milestone gives a **counter and delays**, not preemption. A periodic tick additionally needs
//! the BCM2836 interrupt controller to route the timer IRQ, which is the next step.

use core::sync::atomic::{AtomicU32, Ordering};

use super::pl011_write;
use super::exceptions::write_hex32;

/// The counter's ACTUAL rate in Hz, measured at boot against the System Timer.
///
/// Deliberately not `CNTFRQ`: on the Raspberry Pi 2 that register lies by 19.2x (see `selftest`).
/// Everything that converts counter ticks to time must use this, so a board whose firmware
/// misreports its own timer cannot silently skew every duration in the kernel.
static TIMER_HZ: AtomicU32 = AtomicU32::new(0);

/// The calibrated counter frequency in Hz - the value to divide `cntpct()` deltas by.
///
/// Zero until `init()` has run, which is the honest answer rather than a guess.
pub fn timer_hz() -> u32 {
    TIMER_HZ.load(Ordering::Relaxed)
}

// ---- BCM2835 System Timer: free-running, fixed 1 MHz, in the (Device-mapped) peripheral block. ----
const SYSTIMER_BASE: usize = super::PERIPHERAL_BASE + 0x3000;
const SYSTIMER_CLO: *const u32 = (SYSTIMER_BASE + 0x04) as *const u32;
const SYSTIMER_HZ: u64 = 1_000_000;

/// Read the ARM generic timer frequency (`CNTFRQ`), in Hz.
pub fn cntfrq() -> u32 {
    let f: u32;
    // SAFETY: `mrc p15, 0, _, c14, c0, 0` reads CNTFRQ, a plain read of a PL1-readable system
    // register with no side effects.
    unsafe {
        core::arch::asm!("mrc p15, 0, {f}, c14, c0, 0", f = out(reg) f, options(nomem, nostack));
    }
    f
}

/// Read the 64-bit ARM physical counter (`CNTPCT`).
///
/// The `isb` matters: without it the counter read can be reordered against surrounding instructions,
/// which is harmless for coarse timing and wrong for anything measuring short intervals.
pub fn cntpct() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: `mrrc p15, 0, _, _, c14` reads the 64-bit CNTPCT into a register pair - a side-effect
    // free read of a PL1-readable counter. The ISB orders it against preceding instructions.
    unsafe {
        core::arch::asm!(
            "isb",
            "mrrc p15, 0, {lo}, {hi}, c14",
            lo = out(reg) lo,
            hi = out(reg) hi,
            options(nomem, nostack),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Read the BCM2835 System Timer's low 32 bits (1 MHz, wraps every ~71 minutes).
///
/// Only the low word is used: every interval measured here is far below the wrap period, and reading
/// the 64-bit pair correctly needs a hi/lo/hi re-read dance that buys nothing at this granularity.
/// Wrap is handled by `wrapping_sub` at the call sites.
fn systimer_lo() -> u32 {
    // SAFETY: SYSTIMER_CLO is the BCM2835 System Timer counter-low register, inside the peripheral
    // range the MMU maps as Device memory (see `mmu.rs`). A volatile read of a free-running counter.
    unsafe { SYSTIMER_CLO.read_volatile() }
}

/// Read a register in the BCM2836 core-local peripheral block (`0x4000_0000`).
///
/// This block is BCM2836-specific (a Pi 1 has none) and holds the core timer control and prescaler,
/// the per-core timer/mailbox interrupt routing, and the mailboxes themselves - so the interrupt
/// controller work lands here next.
fn local_reg(off: usize) -> u32 {
    // SAFETY: The core-local block at 0x4000_0000 is mapped Device by `mmu.rs`. A volatile read of a
    // control register; `off` is a caller-supplied constant well inside the mapped 16 MiB.
    unsafe { ((0x4000_0000usize + off) as *const u32).read_volatile() }
}

/// Busy-wait for `us` microseconds using the System Timer (the clock whose rate is fixed by
/// hardware, so this is correct even if `CNTFRQ` is wrong).
pub fn delay_us(us: u32) {
    let start = systimer_lo();
    while systimer_lo().wrapping_sub(start) < us {
        core::hint::spin_loop();
    }
}

/// Print a u32 in decimal. Fixed 10-byte stack buffer, no allocation (§26.6.1).
fn write_dec(mut v: u32) {
    if v == 0 {
        pl011_write(b"0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut n = 0;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut out = [0u8; 10];
    for i in 0..n {
        out[i] = buf[n - 1 - i];
    }
    pl011_write(&out[..n]);
}

/// Report the timer configuration and prove the two clocks agree.
pub fn init() {
    let freq = cntfrq();

    pl011_write(b"arm32: generic timer CNTFRQ = ");
    write_dec(freq);
    pl011_write(b" Hz (");
    write_hex32(freq);
    pl011_write(b")\r\n");

    // CNTFRQ is firmware-programmed, not hardware-discovered. Zero means firmware never set it, and
    // every duration derived from it would be silently wrong - so say so loudly (invariant 12) rather
    // than quietly computing nonsense. The System Timer still works, so this is degraded, not fatal.
    if freq == 0 {
        pl011_write(b"arm32: WARNING - CNTFRQ is 0: firmware did not program it. Generic-timer\r\n");
        pl011_write(b"       durations are UNUSABLE; falling back to the 1 MHz System Timer.\r\n");
        return;
    }

    selftest(freq);
}

/// Measure the two clocks against each other over a fixed interval.
///
/// The System Timer is the reference: its 1 MHz rate is fixed in hardware, whereas `CNTFRQ` is a
/// register firmware writes. So this measures the generic timer's *actual* rate against a known one
/// and compares it with what `CNTFRQ` claims. Agreement means both clocks and the claimed frequency
/// are mutually consistent; disagreement means `CNTFRQ` is lying, which is precisely the failure that
/// would otherwise stay hidden until timing bugs appeared much later.
///
/// 100 ms is long enough that a 1 MHz reference gives ~100,000 ticks of resolution (so the ratio is
/// meaningful) and short enough to be invisible at boot. The tolerance is 1%: the two clocks derive
/// from different dividers off the same crystal, so they should agree closely, but demanding exact
/// equality would make this a flaky test rather than a meaningful one.
fn selftest(freq: u32) {
    const INTERVAL_US: u32 = 100_000; // 100 ms

    let sys_start = systimer_lo();
    let arm_start = cntpct();
    delay_us(INTERVAL_US);
    let arm_elapsed = cntpct() - arm_start;
    let sys_elapsed = systimer_lo().wrapping_sub(sys_start) as u64;

    // Monotonicity first: a counter that does not advance fails everything downstream.
    if arm_elapsed == 0 {
        pl011_write(b"arm32: timer selftest FAIL - CNTPCT did not advance (counter dead?)\r\n");
        return;
    }
    if sys_elapsed == 0 {
        pl011_write(b"arm32: timer selftest FAIL - System Timer did not advance\r\n");
        return;
    }

    // What the generic timer's rate ACTUALLY is, measured against the 1 MHz reference.
    let measured_hz = arm_elapsed * SYSTIMER_HZ / sys_elapsed;
    let claimed_hz = freq as u64;

    // What the BCM2836's own prescaler says the rate SHOULD be. The core timer is fed
    // `source * prescaler / 2^31`, where the source is the crystal when control bit 8 is clear.
    // On this board that is the register which explains the whole discrepancy.
    let prescaler = local_reg(0x08) as u64;
    let expected_from_prescaler = if prescaler != 0 {
        claimed_hz * prescaler / (1u64 << 31)
    } else {
        claimed_hz // no prescaler modelled (QEMU): expect CNTFRQ to be truthful
    };

    let near = |a: u64, b: u64| {
        let d = if a > b { a - b } else { b - a };
        b != 0 && d * 100 <= b
    };
    let within_1pct = near(measured_hz, claimed_hz);
    let explained_by_prescaler = near(measured_hz, expected_from_prescaler);

    pl011_write(b"arm32: timer selftest - CNTFRQ claims ");
    write_dec(claimed_hz as u32);
    pl011_write(b" Hz, measured ");
    write_dec(measured_hz as u32);
    pl011_write(b" Hz vs the 1 MHz System Timer\r\n");
    pl011_write(b"       raw deltas over 100 ms: CNTPCT +");
    write_dec(arm_elapsed as u32);
    pl011_write(b", SysTimer +");
    write_dec(sys_elapsed as u32);
    pl011_write(b"\r\n");

    // The BCM2836 feeds the ARM generic timer through its own control register and prescaler in the
    // core-local block, so the counter's real rate is `source * prescaler / 2^31` - NOT necessarily
    // what CNTFRQ advertises. Print both: if the two clocks disagree, this is the register that
    // explains why, and a porter should not have to go hunting for it.
    pl011_write(b"       BCM2836 core timer control ");
    write_hex32(local_reg(0x00));
    pl011_write(b", prescaler ");
    write_hex32(local_reg(0x08));
    pl011_write(b"\r\n");

    // Record the rate we will actually use. This is the MEASURED value, never CNTFRQ - on this board
    // CNTFRQ is wrong, and a rate that is wrong by 19.2x would put every delay and every scheduler
    // quantum out by the same factor.
    TIMER_HZ.store(measured_hz as u32, Ordering::Relaxed);

    if within_1pct {
        pl011_write(b"arm32: timer selftest PASS (two independent clocks agree within 1%)\r\n");
    } else if explained_by_prescaler {
        // Detected AND explained. The BCM2836 divides the crystal down via its core timer prescaler
        // (source * prescaler / 2^31) but firmware leaves CNTFRQ advertising the UNDIVIDED crystal.
        // On a Pi 2: 19.2 MHz * 0x06AAAAAB / 2^31 = exactly 1 MHz, so CNTFRQ overstates by 19.2x.
        // Not a fault to halt on - a board quirk now understood - but it must stay visible, because
        // anything that trusted CNTFRQ here would be silently wrong (invariant 12).
        pl011_write(b"arm32: timer selftest PASS with a known BCM2836 quirk - CNTFRQ is WRONG on\r\n");
        pl011_write(b"       this board. Firmware divides the counter via the core timer prescaler\r\n");
        pl011_write(b"       but never updates CNTFRQ, which still advertises the undivided crystal.\r\n");
        pl011_write(b"       Prescaler predicts ");
        write_dec(expected_from_prescaler as u32);
        pl011_write(b" Hz and the measurement agrees. Using the MEASURED rate.\r\n");
    } else {
        // Measured matches NEITHER CNTFRQ nor the prescaler's prediction: genuinely unexplained.
        pl011_write(b"arm32: timer selftest FAIL - measured rate matches neither CNTFRQ nor the\r\n");
        pl011_write(b"       prescaler prediction (");
        write_dec(expected_from_prescaler as u32);
        pl011_write(b" Hz). Durations are NOT trustworthy.\r\n");
    }
}
