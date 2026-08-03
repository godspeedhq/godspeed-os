// SPDX-License-Identifier: GPL-2.0-only
//! ARM generic timer (EL1 physical) for the Raspberry Pi 4.
//!
//! The generic timer is architectural, not Broadcom-specific: `CNTFRQ_EL0` reports the counter rate,
//! `CNTPCT_EL0` is a free-running count, and `CNTP_TVAL_EL0` is a down-counter that raises its
//! interrupt when it crosses zero. That makes this port far shorter than the 32-bit one, which had to
//! deal with the BCM2836's own local timer.
//!
//! **The rate is read from the hardware, never assumed.** The 32-bit port has a scar here: a hardcoded
//! guess at the counter frequency made every sleep and timeout on that board wrong by orders of
//! magnitude, and the bug hid for months because nothing compared the assumption against the register.
//! `CNTFRQ_EL0` is right there; there is no reason to guess.

/// The EL1 physical timer's interrupt is PPI 30 on a GICv2. PPIs are banked per core, which is why a
/// per-core timer needs no routing configuration.
pub const TIMER_PPI: u32 = 30;

/// Counter frequency in Hz, straight from `CNTFRQ_EL0`.
pub fn frequency() -> u64 {
    let f: u64;
    // SAFETY: reading CNTFRQ_EL0 is a side-effect-free system-register read available at EL1.
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack)) };
    f
}

/// The free-running physical counter.
pub fn count() -> u64 {
    let c: u64;
    // SAFETY: reading CNTPCT_EL0 is side-effect-free. `isb` first so the read is not speculated ahead
    // of whatever the caller was timing.
    unsafe {
        core::arch::asm!("isb", "mrs {}, cntpct_el0", out(reg) c, options(nomem, nostack));
    }
    c
}

/// Arm the timer to fire once, `ticks` from now.
pub fn arm(ticks: u64) {
    // SAFETY: writing CNTP_TVAL_EL0 sets the down-counter; CNTP_CTL_EL0 bit 0 enables it and bit 1
    // masks it (left clear so the interrupt is delivered). EL2 granted EL1 access to the physical
    // timer during the boot drop (CNTHCTL_EL2), so neither traps.
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {t}", t = in(reg) ticks, options(nomem, nostack));
        core::arch::asm!("msr cntp_ctl_el0, {c}", c = in(reg) 1u64, options(nomem, nostack));
    }
}

/// Disable the timer.
pub fn disable() {
    // SAFETY: clearing CNTP_CTL_EL0 stops the timer raising its interrupt.
    unsafe { core::arch::asm!("msr cntp_ctl_el0, {c}", c = in(reg) 0u64, options(nomem, nostack)) };
}

/// Start a periodic tick at `hz`, via the GIC.
///
/// Returns the reload interval in counter ticks, so the IRQ handler can re-arm with the same value and
/// the caller can report what it actually programmed rather than what it asked for.
pub fn start(hz: u64) -> u64 {
    let interval = frequency() / hz;
    super::gic::enable(TIMER_PPI);
    arm(interval);
    interval
}

/// Unmask IRQs at EL1. Nothing is delivered until this happens, however well the GIC is configured -
/// a detail that reliably costs an hour when the controller looks perfect and no interrupt arrives.
pub fn unmask_irqs() {
    // SAFETY: clearing PSTATE.I permits IRQ delivery; the vectors are installed by this point.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) };
}
