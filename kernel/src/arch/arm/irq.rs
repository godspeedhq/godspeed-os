// SPDX-License-Identifier: GPL-2.0-only
//! BCM2836 interrupt controller - routing the timer IRQ so the counter becomes a *tick*.
//!
//! This is what turns the timer from a thing you can read into a thing that interrupts you, which is
//! the prerequisite for preemption and therefore for tasks.
//!
//! **The Pi 2 has two interrupt controllers, and the choice between them matters.**
//!
//! - The **BCM2835 legacy controller** (`peripheral + 0xB000`) handles *peripheral* interrupts - the
//!   UART, the System Timer, USB, and so on. Shared by all cores, with no per-core routing at all.
//! - The **BCM2836 core-local block** (`0x4000_0000`) is new in the Pi 2 and handles *per-core*
//!   sources: the ARM generic timers, the four mailboxes (used for SMP wakeups), and a funnel for
//!   everything the legacy controller raises.
//!
//! The ARM generic timer is per-core by construction - each core has its own `CNTP_TVAL` - so its
//! interrupt is routed through the core-local block. That is what this module programs. The legacy
//! controller is left alone until something needs a peripheral interrupt (a UART RX IRQ, say).
//!
//! **This is not a GIC.** A Pi 4 (BCM2711) has a GIC-400, which is a completely different programming
//! model. Nothing here transfers to the AArch64 port - another instance of the two ARM ports sharing
//! no code.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::pl011_write;

const LOCAL_BASE: usize = 0x4000_0000;

/// Per-core timer interrupt control. One register per core at `+0x40 + 4*core`.
///
/// Bits 0-3 route the four generic timers to IRQ, bits 4-7 route the same to FIQ:
/// 0 = CNTPS (secure physical), **1 = CNTPNS (non-secure physical)**, 2 = CNTHP (hypervisor),
/// 3 = CNTV (virtual).
const CORE_TIMER_IRQCNTL: usize = LOCAL_BASE + 0x40;

/// Per-core IRQ source (read to discover what fired), at `+0x60 + 4*core`. Same bit assignment as
/// above for the timers, then mailboxes 0-3 in bits 4-7, GPU in bit 8, PMU in bit 9.
const CORE_IRQ_SOURCE: usize = LOCAL_BASE + 0x60;

/// `CNTP_TVAL`/`CNTP_CTL` address **the secure or the non-secure physical timer depending on which
/// security state the CPU is in**, and those are two different interrupt sources here: `CNTPSIRQ`
/// (bit 0) and `CNTPNSIRQ` (bit 1).
///
/// We cannot assume which one we get. The Pi firmware enters an ARMv7 kernel in HYP, which is
/// non-secure, so hardware raises bit 1. QEMU's `raspi2b` stub instead passes through the secure
/// monitor and hands over in *secure* SVC, so it raises bit 0. Routing only the non-secure bit is
/// what made the first version count zero interrupts while `CNTP_CTL.ISTATUS` showed the timer
/// merrily firing - the condition was asserted, nothing was listening for it.
///
/// So route and accept **both**, exactly as `_start` accepts either HYP or SVC entry. One image,
/// either security state, no assumption to be wrong about.
const IRQ_CNTPS: u32 = 1 << 0;
const IRQ_CNTPNS: u32 = 1 << 1;
const IRQ_PHYS_TIMER: u32 = IRQ_CNTPS | IRQ_CNTPNS;

/// Ticks counted since the timer started. The scheduler's future heartbeat; for now it is what the
/// selftest measures to prove the interrupt actually fires at the requested rate.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Counter reload, in timer ticks, for the requested tick period. Written once at setup and read by
/// the ISR on every fire - the down-counter must be re-armed each time or the timer fires once.
static RELOAD: AtomicU32 = AtomicU32::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

fn local_write(addr: usize, v: u32) {
    // SAFETY: The BCM2836 core-local block is mapped Device by `mmu.rs`. Volatile write to a
    // control register at a fixed, in-range offset.
    unsafe { (addr as *mut u32).write_volatile(v) }
}

fn local_read(addr: usize) -> u32 {
    // SAFETY: As above; a volatile read of a status register in the Device-mapped core-local block.
    unsafe { (addr as *const u32).read_volatile() }
}

/// Program `CNTP_TVAL` - the down-counter. Writing it also *clears* a pending timer condition, which
/// is how the ISR acknowledges the interrupt: there is no separate ack register.
fn set_tval(ticks: u32) {
    // SAFETY: `mcr p15, 0, _, c14, c2, 0` writes CNTP_TVAL, a PL1-accessible timer register. Its only
    // effect is to reload the down-counter (and thereby deassert a pending timer interrupt).
    unsafe {
        core::arch::asm!("mcr p15, 0, {t}, c14, c2, 0", t = in(reg) ticks, options(nomem, nostack));
    }
}

/// Enable the physical timer with its interrupt unmasked (`CNTP_CTL`: bit 0 ENABLE, bit 1 IMASK).
fn enable_timer() {
    // SAFETY: `mcr p15, 0, _, c14, c2, 1` writes CNTP_CTL at PL1. Setting ENABLE with IMASK clear
    // arms the timer; the interrupt it raises is routed by the core-local block programmed above, and
    // the vector table is already installed (`exceptions::install` runs earlier in boot).
    unsafe {
        core::arch::asm!("mcr p15, 0, {c}, c14, c2, 1", c = in(reg) 1u32, options(nomem, nostack));
    }
}

/// Read `CNTP_CTL` - bit 0 ENABLE, bit 1 IMASK, bit 2 ISTATUS (the timer condition itself).
fn cntp_ctl() -> u32 {
    let c: u32;
    // SAFETY: `mrc p15, 0, _, c14, c2, 1` reads CNTP_CTL, a side-effect-free PL1 register read.
    unsafe {
        core::arch::asm!("mrc p15, 0, {c}, c14, c2, 1", c = out(reg) c, options(nomem, nostack));
    }
    c
}

/// Read `CPSR` - to check whether IRQs are masked (bit 7) and which mode we are in (low 5 bits).
fn read_cpsr() -> u32 {
    let c: u32;
    // SAFETY: `mrs` reading CPSR is a plain, side-effect-free register read.
    unsafe {
        core::arch::asm!("mrs {c}, cpsr", c = out(reg) c, options(nomem, nostack));
    }
    c
}

/// Unmask IRQs on this core (`CPSR.I = 0`).
pub fn enable_interrupts() {
    // SAFETY: `cpsie i` clears the CPSR I bit. Safe here because the vector table is installed, the
    // IRQ mode has its own stack (`exceptions::install`), and a handler exists for the only source
    // that can fire - so an interrupt now has somewhere well-defined to go.
    unsafe { core::arch::asm!("cpsie i", options(nomem, nostack)) }
}

/// Mask IRQs on this core (`CPSR.I = 1`).
pub fn disable_interrupts() {
    // SAFETY: `cpsid i` sets the CPSR I bit; masking interrupts is always architecturally valid.
    unsafe { core::arch::asm!("cpsid i", options(nomem, nostack)) }
}

/// The Rust side of the IRQ vector. Called from `stub_irq` with caller-saved registers already
/// stacked, and **it returns** - unlike every other exception handler in this port, which halts.
///
/// Kept deliberately small: read the source, handle what we know, re-arm. Anything unrecognised is
/// counted but not acted on, because silently *clearing* an interrupt we do not understand would turn
/// a diagnosable fault into an invisible one.
#[no_mangle]
pub(super) extern "C" fn arm_irq_dispatch() {
    let source = local_read(CORE_IRQ_SOURCE);

    if source & IRQ_PHYS_TIMER != 0 {
        // Re-arm first: writing TVAL both sets the next deadline and deasserts the current interrupt.
        // Doing it before the bookkeeping keeps the period honest - the next interval starts counting
        // from here, not from whenever the handler happens to finish.
        set_tval(RELOAD.load(Ordering::Relaxed));
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    // Other sources (mailboxes, GPU funnel) are not enabled yet, so nothing else should arrive. If
    // something does, leaving it asserted is the loud outcome: it will re-enter and be obvious,
    // rather than being quietly discarded.
}

/// Route the generic timer to this core's IRQ line and start ticking at `hz`.
///
/// Returns false if the timer frequency is unknown, in which case the caller has nothing to program a
/// period from and must not pretend otherwise.
pub fn start_tick(hz: u32) -> bool {
    let timer_hz = super::timer::timer_hz();
    if timer_hz == 0 || hz == 0 {
        pl011_write(b"arm32: cannot start tick - timer frequency unknown\r\n");
        return false;
    }

    // Note this uses the MEASURED frequency, not CNTFRQ. On the Pi 2 those differ by 19.2x, so a tick
    // programmed from CNTFRQ would run 19.2x slow - see `timer.rs`.
    let reload = timer_hz / hz;
    RELOAD.store(reload, Ordering::Relaxed);

    // Core 0 only for now; the other three are still parked in `_start`.
    local_write(CORE_TIMER_IRQCNTL, IRQ_PHYS_TIMER);

    set_tval(reload);
    enable_timer();
    enable_interrupts();
    true
}

/// Prove the tick actually fires, and at the rate requested.
///
/// Same discipline as the timer and MMU selftests: measure against the independent 1 MHz System
/// Timer rather than trusting that programming the registers worked. A tick that never fires and a
/// tick that fires at the wrong rate are different failures, and this separates them.
pub fn selftest(hz: u32) {
    const WINDOW_US: u32 = 500_000; // 500 ms

    let before = ticks();
    super::timer::delay_us(WINDOW_US);
    let fired = ticks() - before;

    let expected = (hz as u64) * (WINDOW_US as u64) / 1_000_000;

    pl011_write(b"arm32: tick selftest - ");
    super::timer::write_dec_pub(fired as u32);
    pl011_write(b" interrupts in 500 ms, expected ~");
    super::timer::write_dec_pub(expected as u32);
    pl011_write(b"\r\n");

    if fired == 0 {
        // Separate "the timer never reached its deadline" from "it did, but the interrupt was not
        // delivered". CNTP_CTL.ISTATUS (bit 2) is asserted by the timer itself, independently of any
        // routing - so if ISTATUS is set while no interrupt arrived, the timer is fine and the fault
        // is in delivery (routing bits, secure/non-secure state, or CPSR.I).
        pl011_write(b"arm32: tick selftest FAIL - the timer IRQ never fired.\r\n");
        pl011_write(b"       CNTP_CTL = ");
        super::exceptions::write_hex32(cntp_ctl());
        pl011_write(b" (bit0 ENABLE, bit1 IMASK, bit2 ISTATUS)\r\n");
        pl011_write(b"       core IRQ source = ");
        super::exceptions::write_hex32(local_read(CORE_IRQ_SOURCE));
        pl011_write(b", routing = ");
        super::exceptions::write_hex32(local_read(CORE_TIMER_IRQCNTL));
        pl011_write(b"\r\n       CPSR = ");
        super::exceptions::write_hex32(read_cpsr());
        pl011_write(b" (bit7 I = IRQs masked; low 5 bits = mode)\r\n");
        return;
    }

    // 25% tolerance, and deliberately loose. Two reasons, both about what this test is FOR.
    //
    // First, the window is measured by a busy-wait that the interrupts themselves preempt, so a few
    // ticks of slop at either end are expected. Second - and this is what set the number - QEMU's TCG
    // timing wanders: the same image measured 45 and then 43 across rebuilds, while real hardware
    // returns exactly 50 every time. A 10% bar failed on emulation jitter alone, and a test that
    // cries wolf gets ignored, which is worse than not having it.
    //
    // The failures worth catching are gross, not subtle: a tick that never fires (handled above) or
    // one off by a factor - like a period computed from CNTFRQ, which would land near 2 instead of 50.
    // Both are far outside 25%. This is a smoke test for "the tick runs at roughly the rate asked
    // for", not a precision measurement, and it should not pretend otherwise.
    let diff = if fired > expected { fired - expected } else { expected - fired };
    if diff * 4 <= expected {
        pl011_write(b"arm32: tick selftest PASS (timer IRQ fires at the requested rate)\r\n");
    } else {
        pl011_write(b"arm32: tick selftest FAIL - the IRQ fires, but at the wrong rate.\r\n");
    }
}
