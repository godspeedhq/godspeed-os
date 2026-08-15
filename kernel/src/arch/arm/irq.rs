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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::pl011_write;

const LOCAL_BASE: usize = 0x4000_0000;

/// The BCM2835 **legacy** peripheral interrupt controller (`peripheral + 0xB200`). It is the only path
/// by which a peripheral IRQ - the USB/DWC2 controller among them - reaches a core, funnelled through
/// the BCM2836 core-local block's "GPU" source (bit 8). Left dormant until the USB stack went
/// interrupt-driven; the timer needs only the core-local block above.
const PERIPH_BASE: usize = 0x3F00_0000;
const IC_PENDING_1:     usize = PERIPH_BASE + 0xB204; // IRQ pending, lines 0-31
const IC_ENABLE_IRQS_1: usize = PERIPH_BASE + 0xB210; // enable IRQ lines 0-31 (write 1 to enable)
const IC_DISABLE_IRQS_1: usize = PERIPH_BASE + 0xB21C; // disable IRQ lines 0-31 (write 1 to disable)
/// The DWC2 OTG controller is peripheral IRQ line 9 on the BCM283x.
const USB_IRQ_LINE: u32 = 9;
/// BCM2836 core-local: route the GPU IRQ (the OR of all legacy peripheral IRQs) and FIQ to a core.
/// Bits 0-1 = the core that receives the GPU IRQ; bits 2-3 = the core that receives the GPU FIQ.
const GPU_INT_ROUTING: usize = LOCAL_BASE + 0x0C;
/// `CORE_IRQ_SOURCE` bit 8: a GPU (legacy-controller peripheral) interrupt is pending on this core.
const CORE_IRQ_GPU: u32 = 1 << 8;

/// Route the DWC2 USB interrupt to core 0 and enable it in the legacy controller.
///
/// Two hops, because the Pi 2 has two interrupt controllers (see the module header): the legacy
/// controller must be told to raise line 9 at all, and the core-local block must be told which core the
/// resulting GPU funnel lands on. Core 0 is the single DWC2 poller/owner, so both point there. The USB
/// interrupt is level-triggered - it stays asserted until its underlying condition is cleared (an HPRT
/// change bit, or a channel's HCINT) - so the handler MUST clear what it services or the line re-fires
/// forever. That is why channel interrupts are gated at HAINTMSK to only the channels the ISR actually
/// drives (`dwc2::init`): a polled channel left with a pending HCINT would storm this line.
pub fn route_usb_irq_to_core0() {
    // GPU IRQ -> core 0 (leave FIQ routing at core 0 too; we do not use USB FIQ).
    local_write(GPU_INT_ROUTING, 0);
    // Enable peripheral line 9 (USB) in the legacy controller's bank-1 enable register.
    // SAFETY: the legacy IC is in the Device-mapped peripheral window; a volatile write that sets one
    // enable bit. Writing 1s enables; 0s are ignored (the register is not read-modify-write).
    unsafe { (IC_ENABLE_IRQS_1 as *mut u32).write_volatile(1 << USB_IRQ_LINE); }
    // NOTE: this whole function has NO CALLERS. USB is enabled through `unmask_usb_irq` from the
    // IrqUnmask syscall instead, and the GPU funnel reaches core 0 by the routing register's reset
    // default rather than by the write above. Discovered while wiring the system timer, whose enable
    // was put here and therefore never ran. Left in place because it documents the intended routing,
    // but nothing may be added here expecting it to execute.
}

/// The NEUTRAL vector a userspace USB driver is granted for this controller.
///
/// The number began life as an x86 MSI vector and is deliberately reused here, exactly as the
/// AArch64 port reuses `0x28` for its xHCI: the arch layer maps its own interrupt onto a shared
/// name, so a driver's CONTRACT says the same thing on every architecture. That is what the neutral
/// routing seam is for. 0x29 is the vector the x86 EHCI uses, and DWC2 is this board's equivalent
/// full/high-speed host controller.
pub const USB_VECTOR: u8 = 0x29;

/// Does a USERSPACE service own the USB controller?
///
/// THE one predicate for that question, so ownership cannot be decided two different ways.
/// Registration for `USB_VECTOR` is the fact; everything else follows from it - the IRQ dispatch
/// routes to whoever registered, and the in-kernel driver's periodic hooks stand down when someone
/// has. There is no separate flag, because a second copy of a fact is a second chance to disagree
/// with it (Commandment III).
///
/// It is also what makes the transition reversible from the prompt: `spawn dwc2` quiets the kernel
/// driver, `kill dwc2` (which calls `route::unregister`) hands the hardware straight back.
pub fn usb_owned_by_userspace() -> bool {
    crate::interrupt::route::registered_endpoint(USB_VECTOR).is_some()
}

/// Mask the USB line at the legacy controller.
///
/// Required before handing this interrupt to userspace. The DWC2 line is LEVEL-triggered - it stays
/// asserted until the driver clears the underlying HPRT change bit or channel HCINT - so an
/// unmasked line re-fires the instant the handler returns and the core makes no further progress,
/// which the liveness watchdog turns into a panic. The in-kernel driver got away with never masking
/// because it cleared the condition inline, before returning; a userspace driver cannot, because it
/// has not run yet.
pub fn mask_usb_irq() {
    // SAFETY: volatile write of one bit to the Device-mapped legacy IC disable register. Writing 1
    // disables that line; 0s are ignored (not read-modify-write), so this cannot disturb other lines.
    unsafe { (IC_DISABLE_IRQS_1 as *mut u32).write_volatile(1 << USB_IRQ_LINE); }
}

/// Unmask the USB line. The counterpart to `mask_usb_irq`, reached from the `IrqUnmask` syscall once
/// the userspace driver has acknowledged the device.
pub fn unmask_usb_irq() {
    // SAFETY: as above, against the enable register.
    unsafe { (IC_ENABLE_IRQS_1 as *mut u32).write_volatile(1 << USB_IRQ_LINE); }
}

/// Tasks waiting on the microsecond one-shot, and when each is due (absolute System Timer counts).
///
/// EIGHT, fixed. The first version had ONE slot and hardware showed what that costs: a 125 us sleep
/// returned in 144 us when it got the slot and 8197 us on average when it did not, because any other
/// sub-tick sleeper in the system took it and everyone else fell back to the 10 ms tick. One slot is
/// not a timer, it is a lottery.
///
/// A fixed array rather than a heap queue keeps the bound readable straight off the source (§26.6.1),
/// and a full table degrades to the tick - which is exactly the behaviour that was there before, so
/// the failure mode is "coarse", never "wrong".
struct HiRes {
    slot: [u32; HIRES_MAX],
    due: [u32; HIRES_MAX],
}
const HIRES_MAX: usize = 8;
static HIRES: crate::smp::spinlock::SpinLock<HiRes> =
    crate::smp::spinlock::SpinLock::new(HiRes { slot: [u32::MAX; HIRES_MAX], due: [0; HIRES_MAX] });

/// Has `now` reached `due`? Wrapping-safe: the System Timer is 32-bit at 1 MHz, so it wraps about
/// every 71 minutes, and a plain `>=` would sleep through the wrap for the better part of an hour.
fn reached(now: u32, due: u32) -> bool {
    now.wrapping_sub(due) < 0x8000_0000
}

/// Re-arm the compare for the EARLIEST outstanding deadline. Called with the lock held.
fn hires_rearm(q: &HiRes) {
    let now = super::timer::systimer_lo();
    let mut best: Option<u32> = None;
    for i in 0..HIRES_MAX {
        if q.slot[i] == u32::MAX {
            continue;
        }
        let d = q.due[i];
        if best.map_or(true, |b| d.wrapping_sub(now) < b.wrapping_sub(now)) {
            best = Some(d);
        }
    }
    if let Some(d) = best {
        // At least 1 us out: programming a deadline already past would rely on the compare firing on
        // an equality it has already gone by, and the entry would never be woken by the timer at all.
        let delta = d.wrapping_sub(now);
        super::timer::arm_oneshot_at(now.wrapping_add(if delta == 0 || delta >= 0x8000_0000 { 1 } else { delta }));
    }
}

/// Where short sleeps actually GO.
///
/// Four hypotheses about one userspace number have now each been wrong, because that number cannot
/// tell "never took the hi-res path" from "took it and the interrupt never came" from "was woken and
/// not run". Those are three different bugs with three different fixes and one symptom. These count
/// them, and the tally prints itself every 16 arms - so the next boot answers the question instead of
/// narrowing it.
static HR_ARM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HR_ELAPSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HR_FULL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HR_IRQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HR_WOKE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn hires_report() {
    crate::kprintln!(
        "arm32: hi-res tally - {} armed, {} elapsed-while-arming, {} table-full, {} compare IRQs, {} woken",
        HR_ARM.load(Ordering::Relaxed), HR_ELAPSED.load(Ordering::Relaxed),
        HR_FULL.load(Ordering::Relaxed), HR_IRQ.load(Ordering::Relaxed),
        HR_WOKE.load(Ordering::Relaxed));
}

/// What arming a short sleep concluded.
pub enum Armed {
    /// Registered; the caller should block and will be woken by the compare interrupt.
    Pending,
    /// The requested time ELAPSED while we were arming it. The caller must NOT block.
    ///
    /// This is the whole short-sleep bug. The compare fires on EQUALITY, and every System Timer
    /// access is an uncached Device read, so programming a 125 us deadline can itself take longer
    /// than 125 us. The counter is then already past the value written, the match never happens, and
    /// the task waits out the 10 ms tick backstop instead. Measured on hardware exactly as that
    /// predicts - 2000 us sleeps land within 28 us, 125 us sleeps average 8160 us.
    ///
    /// Returning immediately is not an approximation, it is the correct answer: the caller asked to
    /// wait 125 us and 125 us has passed.
    Elapsed,
    /// No free entry; the caller falls back to the tick, as it did before this existed.
    Full,
}

/// Register `slot` to wake in `us` microseconds.
pub fn hires_arm(slot: u32, us: u32) -> Armed {
    let mut q = HIRES.lock();
    let free = (0..HIRES_MAX).find(|&i| q.slot[i] == u32::MAX);
    let i = match free {
        Some(i) => i,
        None => { HR_FULL.fetch_add(1, Ordering::Relaxed); return Armed::Full; }
    };
    let due = super::timer::systimer_lo().wrapping_add(us.max(1));
    q.slot[i] = slot;
    q.due[i] = due;
    hires_rearm(&q);
    // RE-READ AFTER ARMING. Everything above - the lock, the counter read, the compare write - takes
    // time, and for a short deadline that time can be the whole delay. Checking afterwards catches
    // precisely the case the hardware cannot: a deadline that went by while we were setting it.
    if reached(super::timer::systimer_lo(), due) {
        q.slot[i] = u32::MAX;
        hires_rearm(&q);
        HR_ELAPSED.fetch_add(1, Ordering::Relaxed);
        return Armed::Elapsed;
    }
    let n = HR_ARM.fetch_add(1, Ordering::Relaxed) + 1;
    drop(q);                       // never print holding the queue lock
    // ONCE per boot, at 64, and never again.
    //
    // This printed every 16 arms - and a sweep is exactly 16 sleeps per duration, so every single
    // measurement had a ~9 ms kernel log line dropped into the middle of it. The instrument was
    // producing the number it was meant to be measuring: the quiet MAX sat at ~6900 us across two
    // different placements because that IS one log line, not because anything was stalling.
    //
    // Same mistake as the wake-latency probe that blocked this service's bring-up for two minutes.
    // A diagnostic on a path it is timing has to be rarer than the thing it measures, or it becomes
    // the thing it measures.
    if n == 64 { hires_report(); }
    Armed::Pending
}

/// Drop `slot` from the table (on wake, or on any early exit).
pub fn hires_release(slot: u32) {
    let mut q = HIRES.lock();
    for i in 0..HIRES_MAX {
        if q.slot[i] == slot {
            q.slot[i] = u32::MAX;
        }
    }
    hires_rearm(&q);
}

/// Wake everything now due, then re-arm for the next. Called from the IRQ handler.
fn hires_fire() -> bool {
    HR_IRQ.fetch_add(1, Ordering::Relaxed);
    let mut woken = [u32::MAX; HIRES_MAX];
    {
        let mut q = HIRES.lock();
        let now = super::timer::systimer_lo();
        for i in 0..HIRES_MAX {
            if q.slot[i] != u32::MAX && reached(now, q.due[i]) {
                woken[i] = q.slot[i];
                q.slot[i] = u32::MAX;
            }
        }
        hires_rearm(&q);
    }
    // Wake OUTSIDE the lock: the scheduler takes its own locks, and holding two at once is how a
    // deadlock is built. The entries are already removed, so a concurrent arm cannot collide.
    let mut any = false;
    for w in woken.iter() {
        if *w != u32::MAX {
            crate::task::scheduler::wake_by_slot(*w as usize, 0);
            HR_WOKE.fetch_add(1, Ordering::Relaxed);
            any = true;
        }
    }
    any
}

/// Doorbells received, per core./// Doorbells received, per core. Exists so the boot selftest can prove the path end to end on the
/// machine actually running, rather than trusting that a register write meant something.
static DOORBELLS: [core::sync::atomic::AtomicU32; 4] = [
    core::sync::atomic::AtomicU32::new(0), core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0), core::sync::atomic::AtomicU32::new(0),
];

/// How many doorbells `core` has taken and cleared.
pub fn doorbells_received(core: u32) -> u32 {
    DOORBELLS[(core & 3) as usize].load(Ordering::Relaxed)
}

/// Ring one core's mailbox-0 doorbell, asserting its IRQ so it reschedules NOW.
///
/// Bounded and idempotent: the mailbox is a bitmap, so ringing a core that has not yet drained its
/// doorbell leaves the same bit set rather than queueing anything.
pub fn ring_doorbell(core: u32) {
    if core >= 4 {
        return; // four cores on this SoC; a bad index would write into another block's registers
    }
    // SAFETY: volatile write of one bit to a Device-mapped mailbox WRITE-SET register. Write-set
    // semantics ignore 0s, so this cannot disturb a bit another sender has set.
    local_write(CORE_MBOX_WRITE_SET + 16 * core as usize, MBOX_WAKE_BIT);
}

/// True if the legacy controller currently shows the USB line pending (used by the dispatcher to
/// confirm the GPU funnel is USB and not some other peripheral before handing it to the USB stack).
fn usb_irq_pending() -> bool {
    // SAFETY: volatile read of the Device-mapped legacy IRQ-pending register.
    (unsafe { (IC_PENDING_1 as *const u32).read_volatile() }) & (1 << USB_IRQ_LINE) != 0
}

/// Per-core timer interrupt control. One register per core at `+0x40 + 4*core`.
///
/// Bits 0-3 route the four generic timers to IRQ, bits 4-7 route the same to FIQ:
/// 0 = CNTPS (secure physical), **1 = CNTPNS (non-secure physical)**, 2 = CNTHP (hypervisor),
/// 3 = CNTV (virtual).
/// Per-core MAILBOX interrupt control, at `+0x50 + 4*core`. Bits 0-3 enable an IRQ for mailboxes
/// 0-3. This is the BCM2836 inter-processor doorbell, and it is what makes a cross-core wake
/// immediate instead of "whenever that core next takes a timer tick".
const CORE_MBOX_IRQCNTL: usize = LOCAL_BASE + 0x50;
/// Mailbox WRITE-SET, at `+0x80 + 16*core + 4*mbox`. Writing sets bits; the target core's IRQ stays
/// asserted while any bit is set.
const CORE_MBOX_WRITE_SET: usize = LOCAL_BASE + 0x80;
/// Mailbox READ / WRITE-HIGH-TO-CLEAR, at `+0xC0 + 16*core + 4*mbox`. Reading shows the pending bits;
/// writing those same bits back clears them. Clearing is what deasserts the line, so a handler that
/// reads without writing back storms its own core.
const CORE_MBOX_RDCLR: usize = LOCAL_BASE + 0xC0;
/// `CORE_IRQ_SOURCE` bit 4: mailbox 0 has something in it on this core.
const CORE_IRQ_MBOX0: u32 = 1 << 4;
/// One doorbell bit is all a wake needs. The mailbox word is a bitmap, so 31 bits remain for any
/// future signal that must be told apart from "reschedule"; today every IPI vector this port sends
/// means exactly that, so encoding the vector number would store a fact nobody reads.
const MBOX_WAKE_BIT: u32 = 1 << 0;

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

/// The calling core's index (0-3), read from MPIDR. The BCM2836 core-local block has one copy of
/// each interrupt register PER CORE at `+4*core`, so every access below must be indexed by this - a
/// timer IRQ fires on the core whose down-counter expired and is handled there, and reading core 0's
/// source register from core 2 would miss it.
fn this_core() -> usize {
    let mpidr: u32;
    // SAFETY: reading MPIDR (c0,c0,5) is a side-effect-free PL1 register read.
    unsafe { core::arch::asm!("mrc p15, 0, {m}, c0, c0, 5", m = out(reg) mpidr, options(nomem, nostack)); }
    (mpidr & 3) as usize
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
///
/// Once the neutral scheduler is running (`scheduler::run`), the timer tick drives IT (preemptive
/// `switch_context` via `timer_tick_from_irq`) rather than the early `context.rs` demo scheduler. Set
/// by the port when it hands control to `scheduler::run`.
pub static NEUTRAL_SCHED: AtomicBool = AtomicBool::new(false);

/// Per-slot "this is a USER (ring-3) task" flags, maintained arch-locally so the timer can implement
/// **atomic syscalls** (below) without reaching into the neutral scheduler's `static mut TASK_IS_USER`
/// (which would grow `task/`'s grandfathered unsafe floor). The ARM spawn/commit paths mark each user
/// task's slot via `mark_task_user`; kernel tasks (the demos) are left `false` and stay preemptible.
const ARM_MAX_TASKS: usize = 256;
static ARM_TASK_IS_USER: [AtomicBool; ARM_MAX_TASKS] =
    [const { AtomicBool::new(false) }; ARM_MAX_TASKS];

/// Mark scheduler `slot` as a USER task (so the timer won't preempt it mid-syscall). Idempotent.
pub fn mark_task_user(slot: usize) {
    if slot < ARM_MAX_TASKS { ARM_TASK_IS_USER[slot].store(true, Ordering::Relaxed); }
}

#[no_mangle]
pub(super) extern "C" fn arm_irq_dispatch(frame_sp: u32) -> u32 {
    // Per-core source register: the timer fired on THIS core, so read this core's `+0x60 + 4*core`.
    let source = local_read(CORE_IRQ_SOURCE + 4 * this_core());

    // A GPU-funnel interrupt (bit 8) is a legacy-controller peripheral IRQ. The USB stack is the only
    // peripheral IRQ we enable, and it is routed to core 0, so service it here and fall through to the
    // timer check (both can be pending at once). Confirm the line is USB before acting, so an
    // unexpected peripheral IRQ is left asserted and obvious rather than silently swallowed.
    // The microsecond one-shot arrives through the same GPU funnel as USB, so check it here and
    // let the USB test below still run - both can be pending in one interrupt.
    // WAKING IS NOT RUNNING, and forgetting that cost the whole feature.
    //
    // The one-shot arrives through the GPU funnel, not as a timer or mailbox interrupt - so marking a
    // task runnable here did nothing until the core's NEXT 10 ms tick came round to schedule it.
    // Hardware showed it exactly: min 147 us when a tick happened to be imminent, mean 8213 us
    // otherwise, which is half a quantum. The timer was perfect and the wake was on time; the task
    // just sat there Ready. So record that we woke someone and take the scheduling path below.
    let woke_hires = this_core() == 0
        && source & CORE_IRQ_GPU != 0
        && super::timer::take_oneshot_match()
        && hires_fire();

    let handled_gpu = if this_core() == 0 && source & CORE_IRQ_GPU != 0 && usb_irq_pending() {
        // DEVICE INTERRUPTS CAN GO TO USERSPACE ON ARM32. They always could.
        //
        // CLAUDE.md 6.4 (SEC-29/30) justifies the in-kernel DWC2 stack on the grounds that "ARM does
        // not yet route device IRQs to userspace", and treats that as the reason a Commandment I
        // violation has to be accepted on this port. The hardware was never the obstacle: this line
        // is already being received, confirmed and dispatched right here. It was handed to the
        // kernel's own driver because nothing else had ever asked for it.
        //
        // The AArch64 port found the identical claim about the GIC and it was equally untrue - see
        // the comment in `arch/aarch64/exceptions.rs`. Twice now an unimplemented branch has been
        // read as an architectural constraint. Worth stating plainly so it is not read that way a
        // third time.
        //
        // The route is chosen by who has REGISTERED for the vector, not by a build flag. With no
        // userspace driver the in-kernel stack still owns the controller and behaviour is bit-for-bit
        // what it was; the moment a service is granted `hw_irqs = [0x29]` the interrupt goes there
        // instead. That makes the transition testable in one step, and it means there is never a
        // build in which both drivers believe they own the hardware - the failure mode the AArch64
        // feature flag actually produced before it was deleted.
        if usb_owned_by_userspace() {
            // MASK FIRST. The line is level-triggered and the userspace driver has not run yet, so
            // without this it re-asserts immediately and the core never leaves the handler. The
            // driver unmasks through `IrqUnmask` once it has cleared the device.
            mask_usb_irq();
            // SAFETY: in the IRQ handler with interrupts masked - `deliver`'s documented contract.
            unsafe { crate::interrupt::route::deliver(USB_VECTOR) };
        } else {
            // arm32 slice 5: there is no in-kernel USB driver to fall back to. The vector is
            // routed to the `dwc2` service or it is masked; an unclaimed level-triggered line that
            // nothing can clear would storm this core, so say so rather than silently re-enabling it.
            crate::kprintln!("arm: USB IRQ with no userspace driver registered - masking the line");
            mask_usb_irq();
        }
        true
    } else {
        false
    };

    // A DOORBELL FROM ANOTHER CORE. Clear it first, then fall into the scheduler below.
    //
    // This is what the empty `send_ipi_to_lapic` had been throwing away. The scheduler rings it
    // whenever a send makes a task on ANOTHER core runnable; with no doorbell that core carried on
    // until its next 10 ms tick, so every cross-core IPC hop cost up to a whole quantum.
    //
    // Clear BEFORE the work: the line asserts while any mailbox bit is set, so a handler that
    // reschedules first and clears afterwards can be re-entered by its own uncleared doorbell.
    if source & CORE_IRQ_MBOX0 != 0 {
        let mb = CORE_MBOX_RDCLR + 16 * this_core();
        let pending = local_read(mb);
        local_write(mb, pending); // write-high-to-clear: exactly the bits just observed
        DOORBELLS[this_core() & 3].fetch_add(1, Ordering::Relaxed);
    }

    if source & (IRQ_PHYS_TIMER | CORE_IRQ_MBOX0) != 0 || woke_hires {
        // Re-arm first: writing TVAL both sets the next deadline and deasserts the current interrupt.
        // Doing it before the bookkeeping keeps the period honest - the next interval starts counting
        // from here, not from whenever the handler happens to finish. (This is the ARM timer's "EOI";
        // the neutral `apic_send_eoi` is a no-op here.)
        // Only for a real TIMER interrupt. A doorbell shares the scheduling path below but is not a
        // tick: re-arming on it would shorten the quantum, and counting it would make
        // `monotonic_ticks` - which paces every sleep and timeout in the system - run fast in
        // proportion to how much cross-core IPC the machine happens to be doing.
        if source & IRQ_PHYS_TIMER != 0 {
            set_tval(RELOAD.load(Ordering::Relaxed));
            TICKS.fetch_add(1, Ordering::Relaxed);
        }

        // Hands-off chaos demo: Core 0 counts ticks and, once boot has settled, injects the storm
        // command into the input ring (no keyboard needed). One-shot, latched inside.
        #[cfg(feature = "arm-autochaos")]
        if this_core() == 0 { super::autochaos_tick(); }

        if NEUTRAL_SCHED.load(Ordering::Relaxed) {
            // **Atomic syscalls: do not preempt a USER task that is in a syscall (SVC mode).** Unlike
            // x86, preempting ARM kernel/SVC code mid-syscall corrupts - SPSR_svc and the SVC-banked sp
            // are single shared registers, so switching to another task (which runs its own syscall)
            // clobbers state the interrupted syscall must restore at its `movs pc` return, producing a
            // wild-PC fault (proven: slowing the tick to run syscalls to completion eliminates it). A
            // blocking syscall yields *voluntarily* via `block_and_reschedule`, so this cannot let a
            // task monopolise the core; a non-blocking syscall is short; and the task is preempted the
            // instant it is back in USER mode. Only a USER task in SVC is a syscall: a *kernel* task
            // (the demos) runs in SVC as its normal body and MUST stay preemptible, so the check is
            // gated on this slot being a user task, not on SVC mode alone.
            //
            // The interrupted CPSR is the trap frame's `spsr`, the last of its 18 words:
            // [usr_sp, usr_lr, r0..r12, lr_svc, pc, spsr] -> spsr at frame_sp + 68.
            // SAFETY: `frame_sp` is the trap frame `stub_irq` built on the interrupted task's stack.
            let interrupted_spsr = unsafe { ((frame_sp + 68) as *const u32).read_volatile() };
            let in_svc = (interrupted_spsr & 0x1f) != 0x10; // not USR mode -> SVC (kernel/syscall)
            let slot = crate::task::scheduler::current_task_slot();
            let user_task = slot < ARM_MAX_TASKS && ARM_TASK_IS_USER[slot].load(Ordering::Relaxed);
            // Only protect a task that is genuinely *running* a syscall. A task BLOCKED in a syscall
            // (the shell in `console_read`) has voluntarily yielded and the core is idling in its
            // context (current still points at it, in SVC) - if we skipped the tick here too, the timer
            // would NEVER drain the UART RX or reschedule the woken task, so serial input could never
            // arrive. Gating on "running" lets the tick run while blocked (drain + wake) but still keeps
            // an actively-executing syscall atomic.
            let running = crate::task::scheduler::current_task_is_running();
            if !(in_svc && user_task && running) {
                // Preempt: the neutral tick swaps `sp` to the resumed task's kernel stack INTERNALLY,
                // so we return `frame_sp` unchanged and `stub_irq`'s `mov sp, r0` is a no-op. The SAME
                // stub serves both paths (below): the demo scheduler returns a DIFFERENT frame to adopt;
                // the neutral one swaps in place. Runs with IRQs masked (IRQ-mode entry set CPSR.I).
                // SAFETY: `timer_tick_from_irq` is the neutral preemption entry; on ARM it is reached
                // only from this masked IRQ handler running on the interrupted task's kernel stack.
                unsafe { crate::task::scheduler::timer_tick_from_irq(0, 0, 0); }
            }
            return frame_sp;
        }
    }
    // A GPU/USB interrupt that fired without a coincident timer tick still needs the scheduler-safe
    // return once the neutral scheduler owns the core (same contract as the timer branch: the tick
    // handler swaps stacks internally, so we hand back the frame unchanged).
    if handled_gpu && NEUTRAL_SCHED.load(Ordering::Relaxed) {
        return frame_sp;
    }

    // Other sources (mailboxes) are not enabled, so nothing else should arrive. If something does,
    // leaving it asserted is the loud outcome: it will re-enter and be obvious, rather than being
    // quietly discarded.

    // The `context.rs` demo scheduler lives on CORE 0 only (the boot selftests ran there before the
    // neutral scheduler took over). A secondary core (SMP) reaches here only while it idles in
    // `scheduler::run` before `NEUTRAL_SCHED` is set - it has no demo tasks, so just resume it.
    if this_core() != 0 {
        return frame_sp;
    }

    // Pre-scheduler (boot selftests, incl. preempt_selftest): the `context.rs` demo scheduler. It
    // returns the frame to RESUME - the same to continue, or another task's to preempt.
    super::context::schedule(frame_sp)
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

    // Route the generic timer to THIS core's IRQ line (per-core register at +0x40 + 4*core).
    local_write(CORE_TIMER_IRQCNTL + 4 * this_core(), IRQ_PHYS_TIMER);
    // Take mailbox 0 as well: this core must be wakeable by ANOTHER core, not only by its own timer.
    // Enabled here, beside the timer routing, because both answer "what may interrupt this core" and
    // splitting them is how one of them ends up forgotten.
    local_write(CORE_MBOX_IRQCNTL + 4 * this_core(), 1);
    // And the system timer's compare-3 line, which carries the microsecond one-shot through the GPU
    // funnel to core 0. Enabled HERE, in the path that actually runs at boot - it was first put in
    // `route_usb_irq_to_core0`, which turns out to have no callers, so it silently never happened and
    // every sub-tick sleep quietly fell back to the 10 ms tick.
    // SAFETY: volatile write of one enable bit to the Device-mapped legacy IC; 0s are ignored, so
    // other lines are undisturbed.
    unsafe { (IC_ENABLE_IRQS_1 as *mut u32).write_volatile(1 << super::timer::SYSTIMER_C3_IRQ); }

    set_tval(reload);
    enable_timer();
    enable_interrupts();
    true
}

/// Start the tick on a secondary core (SMP). Core 0 already computed and stored `RELOAD` in
/// `start_tick`; an AP only needs to route ITS own timer interrupt (per-core `CORE_TIMER_IRQCNTL`),
/// arm its own banked down-counter, and unmask IRQs on itself. The generic-timer registers
/// (`CNTP_TVAL`/`CNTP_CTL`) are per-core by construction, so `set_tval`/`enable_timer` act on this
/// core alone. Returns false if core 0 never established a reload (timer frequency unknown).
pub fn start_tick_ap(_core: u32) -> bool {
    let reload = RELOAD.load(Ordering::Relaxed);
    if reload == 0 {
        return false;
    }
    local_write(CORE_TIMER_IRQCNTL + 4 * this_core(), IRQ_PHYS_TIMER);
    // Take mailbox 0 as well: this core must be wakeable by ANOTHER core, not only by its own timer.
    // Enabled here, beside the timer routing, because both answer "what may interrupt this core" and
    // splitting them is how one of them ends up forgotten.
    local_write(CORE_MBOX_IRQCNTL + 4 * this_core(), 1);
    // And the system timer's compare-3 line, which carries the microsecond one-shot through the GPU
    // funnel to core 0. Enabled HERE, in the path that actually runs at boot - it was first put in
    // `route_usb_irq_to_core0`, which turns out to have no callers, so it silently never happened and
    // every sub-tick sleep quietly fell back to the 10 ms tick.
    // SAFETY: volatile write of one enable bit to the Device-mapped legacy IC; 0s are ignored, so
    // other lines are undisturbed.
    unsafe { (IC_ENABLE_IRQS_1 as *mut u32).write_volatile(1 << super::timer::SYSTIMER_C3_IRQ); }
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
