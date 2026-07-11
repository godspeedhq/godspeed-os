// SPDX-License-Identifier: GPL-2.0-only
//! Inter-Processor Interrupts - §9.4, §8.4, §10.5.
//!
//! Three distinct purposes:
//!   1. Wake a task blocked on `recv` after a cross-core `send` enqueues.
//!   2. TLB shootdown after a page is unmapped (§10.5).
//!   3. Cross-core scheduler preemption (timer overflow).

use core::sync::atomic::{AtomicU64, Ordering};
use crate::smp::percpu::{PerCore, num_cores};

/// Vector numbers for each IPI purpose.
pub mod vectors {
    pub const WAKE_RECEIVER:  u8 = 0xF0;
    pub const TLB_SHOOTDOWN:  u8 = 0xF1;
    pub const SCHEDULER_TICK: u8 = 0xF2;
}

// xAPIC ICR register offsets (relative to APIC base).
const APIC_ICR_HIGH: u64 = 0x310;
const APIC_ICR_LOW:  u64 = 0x300;

// Per-core TLB shootdown state - one independent request slot per INITIATING core, so concurrent
// shootdowns never share a counter or address. The single-global `TLB_ACK` / `TLB_SHOOTDOWN_ADDR`
// this replaced DEADLOCKED when two cores unmapped at once: each spun IF=0 waiting for the other's
// ack while being a target of the other's all-excluding-self broadcast, so neither could ack the
// other. Now a core waiting for its own acks ALSO services every other core's pending request
// (`service_pending`), so concurrent shootdowns serialize cleanly instead of deadlocking. This was the
// max-carnage wedge at 71K rounds: heavy concurrent reclaims across cores (§10.5).
//
// Both arrays are boot-allocated per-core arenas (`PerCore`, §26.6.1) sized to the cores Limine
// actually reported, NOT a fixed `[_; MAX_CORES]` - a 4-core box reserves 4 slots, not MAX_CORES.
//
//   SHOOTDOWN_ADDR.get(x)     - the VA core x is invalidating (`!0` = full CR3 flush).
//   SHOOTDOWN_ACK_MASK.get(x) - a MAX_WORDS-word bitmask: bit y (word y/64, bit y%64) set = core y has
//                               serviced x's CURRENT request. The initiator CLEARS all words to
//                               publish (a clear bit = "service me"); each servicer sets its own bit;
//                               the initiator waits until every expected bit is set. Initialised
//                               all-set (!0) so a never-requested slot is not spuriously serviced.
//                               MAX_WORDS = ceil(MAX_CORES/64) covers the whole sanity ceiling, so up
//                               to MAX_CORES cores - one bit per (initiator, receiver) pair.

/// u64 words a per-initiator ack bitmask needs to hold one bit per core up to the `MAX_CORES` sanity
/// ceiling: `ceil(MAX_CORES / 64)`.
const MAX_WORDS: usize = crate::smp::core::MAX_CORES.div_ceil(64);

/// Watchdog bound for the shootdown ack-wait spin (see `request_and_wait`). A real shootdown completes
/// in a few thousand iterations even under contention; ~5x10^8 is thousands of times that, so it fires
/// ONLY on a true wedge (a core that will never ack) and turns a silent freeze into a loud, pinpointed
/// panic. At ~1.5-2 GHz this is on the order of a few seconds of spin before the panic.
const SHOOTDOWN_WATCHDOG_SPINS: u64 = 500_000_000;

static SHOOTDOWN_ADDR:     PerCore<AtomicU64>              = PerCore::new();
static SHOOTDOWN_ACK_MASK: PerCore<[AtomicU64; MAX_WORDS]> = PerCore::new();

/// Allocate the per-core shootdown arenas for `n` cores. Called once at boot (`smp::percpu_init`),
/// after the frame allocator is up and before any shootdown can run (before APs / spawn). ADDR starts
/// 0; every ack-mask word starts all-set so a never-requested slot reads "already acked by everyone".
pub fn init_arenas(n: usize) {
    SHOOTDOWN_ADDR.init_with(n, |_| AtomicU64::new(0));
    SHOOTDOWN_ACK_MASK.init_with(n, |_| [const { AtomicU64::new(!0) }; MAX_WORDS]);
}

/// `(word index, bit mask within that word)` for core `c` in a multi-word ack bitmask.
#[inline]
fn word_bit(c: usize) -> (usize, u64) {
    (c / 64, 1u64 << (c % 64))
}

/// The calling core's id - via its LAPIC, staying within smp+arch (no up-call into the scheduler).
#[inline]
fn this_core() -> usize {
    // SAFETY: the APIC is mapped before any IPI path runs.
    let lapic = unsafe { crate::arch::x86_64::boot::get_lapic_id() };
    crate::smp::core::lapic_to_core_id(lapic) as usize
}

/// Invalidate `addr` on THIS core - a single-page `invlpg`, or a full CR3 reload for the `!0` sentinel.
#[inline]
fn invalidate(addr: u64) {
    if addr == !0u64 {
        // SAFETY: reloading CR3 with the same value is always valid in ring 0.
        unsafe {
            let cr3: u64;
            core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
            core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, nomem));
        }
    } else {
        // SAFETY: invlpg is always valid in ring 0; `addr` is a virtual address.
        unsafe { core::arch::asm!("invlpg [{a}]", a = in(reg) addr, options(nostack)); }
    }
}

/// Bitmask of every ready core except `me` - the set of acks an initiator must collect.
fn ready_mask_excluding(me: usize) -> [u64; MAX_WORDS] {
    let mut mask = [0u64; MAX_WORDS];
    for x in 0..num_cores() {
        if x != me && crate::smp::core::is_ready(x as u32) {
            let (w, b) = word_bit(x);
            mask[w] |= b;
        }
    }
    mask
}

/// Service every OTHER ready core's pending shootdown request once: invalidate its VA and ack it.
/// Called from BOTH the IPI handler and the ack-wait spin, so a core waiting for its own acks still
/// acks everyone else's request - the deadlock-breaker. Touches no interrupt state (pure shared-slot
/// polling + invlpg), so it is safe to call IF=0 mid-spin. A slot is "pending for me" exactly when my
/// bit is clear (the initiator cleared the whole mask to publish); after I service it I set my bit.
fn service_pending(me: usize) {
    let (mw, mb) = word_bit(me);
    for x in 0..num_cores() {
        if x == me || !crate::smp::core::is_ready(x as u32) { continue; }
        if SHOOTDOWN_ACK_MASK.get(x)[mw].load(Ordering::SeqCst) & mb == 0 {
            invalidate(SHOOTDOWN_ADDR.get(x).load(Ordering::SeqCst));
            SHOOTDOWN_ACK_MASK.get(x)[mw].fetch_or(mb, Ordering::SeqCst);
        }
    }
}

/// Send a fixed-delivery IPI to a specific core.
///
/// # Safety
/// `core_id` must refer to a ready core; the local APIC must be mapped.
pub unsafe fn send_ipi(core_id: u32, vector: u8) {
    let lapic_id  = crate::smp::core::core_lapic_id(core_id);
    let apic_base = unsafe { crate::arch::x86_64::boot::get_apic_virt_base() };

    // xAPIC ICR write protocol: write high word (destination) first, then
    // low word (vector + delivery mode), which triggers delivery.
    // Fixed delivery, edge-triggered, assert (bit 14), no shorthand.
    // SAFETY: apic_base is a valid MMIO address set during init_local_apic.
    unsafe {
        write_apic_reg(apic_base + APIC_ICR_HIGH, (lapic_id & 0xFF) << 24);
        // Per Intel SDM §10.6.1: poll the Delivery Status bit (bit 12 of
        // ICR_LOW) before writing a new IPI.  Writing while DELIVS=1 silently
        // drops the interrupt on some xAPIC implementations (observed on
        // Goldmont+ under concurrent IPI load).  Cap at 10 000 iterations to
        // avoid an infinite spin if the APIC is wedged.
        let mut tries = 0u32;
        while (read_apic_reg(apic_base + APIC_ICR_LOW) >> 12) & 1 != 0 {
            core::hint::spin_loop();
            tries += 1;
            if tries >= 10_000 { break; }
        }
        write_apic_reg(apic_base + APIC_ICR_LOW, (vector as u32) | (1 << 14));
    }
}

/// Broadcast a `TLB_SHOOTDOWN` IPI to all OTHER cores (all-excluding-self shorthand).
///
/// # Safety
/// The APIC must be mapped; the caller holds IF=0 (or has saved/disabled it).
unsafe fn broadcast_shootdown_ipi() {
    let apic_base = unsafe { crate::arch::x86_64::boot::get_apic_virt_base() };
    // SAFETY: APIC mapped; IF=0.
    unsafe {
        write_apic_reg(apic_base + APIC_ICR_HIGH, 0);
        // Poll DELIVS (bit 12) before the broadcast (SDM §10.6.1) - writing while DELIVS=1 silently
        // drops the IPI on some xAPICs. Bounded so a wedged APIC can't spin forever.
        let mut tries = 0u32;
        while (read_apic_reg(apic_base + APIC_ICR_LOW) >> 12) & 1 != 0 {
            core::hint::spin_loop();
            tries += 1;
            if tries >= 10_000 { break; }
        }
        // ICR shorthand 0b11 (all-excluding-self, bits 19:18), fixed delivery, edge, assert (bit 14).
        write_apic_reg(
            apic_base + APIC_ICR_LOW,
            (vectors::TLB_SHOOTDOWN as u32) | (1 << 14) | (0b11 << 18),
        );
    }
}

/// Publish this core's shootdown request for `addr` (`!0` = full CR3 flush), broadcast it, and wait
/// for every other ready core to service it - servicing THEIRS in the spin (the deadlock-breaker).
///
/// # Safety
/// The APIC must be mapped and the caller holds IF=0. The per-core request slots make concurrent
/// shootdowns safe; the broadcast + spin run with interrupts off as the protocol requires (§10.5).
unsafe fn request_and_wait(addr: u64) {
    if crate::smp::core::ready_count() <= 1 {
        return; // single-core; no remote TLBs to invalidate
    }
    let me = this_core();
    // The acks we must collect: a bit for every OTHER ready core.
    let expected = ready_mask_excluding(me);

    // Publish: set the address, THEN clear every ack-mask word. The clear IS the "new request" signal
    // (a servicer acts on a clear bit), so the address must land first - a servicer seeing its bit
    // clear then reads the new address.
    SHOOTDOWN_ADDR.get(me).store(addr, Ordering::SeqCst);
    for w in 0..MAX_WORDS {
        SHOOTDOWN_ACK_MASK.get(me)[w].store(0, Ordering::SeqCst);
    }

    // SAFETY: APIC mapped; IF=0.
    unsafe { broadcast_shootdown_ipi(); }

    // Wait until every expected core (across all words) has set its bit, servicing theirs meanwhile so
    // two cores shooting down at once ack each other instead of deadlocking.
    //
    // WATCHDOG (invariant 12 / §26.7): the wait is BOUNDED. A real shootdown - even under heavy
    // concurrent-kill contention with the deadlock-breaker serializing - completes in well under a
    // millisecond (a few thousand iterations). If we spin far past that, an expected core is NEVER going
    // to ack: its IPI was lost (the bounded ICR-busy spin above gave up mid-broadcast), or it is wedged
    // IF=0 in a path that never services pending. That used to be a SILENT system-wide freeze (the
    // `chaos max-carnage` wedge on the Wyse's 4 real cores). Now it PANICS loudly, naming the core still
    // waiting and the bitmask of cores that failed to ack - a pinpointed report instead of a dead
    // machine. The bound is ~5000x a normal shootdown, so it cannot false-fire on legitimate load.
    let mut spins: u64 = 0;
    loop {
        service_pending(me);
        let done = (0..MAX_WORDS).all(|w| {
            let acked = SHOOTDOWN_ACK_MASK.get(me)[w].load(Ordering::SeqCst);
            acked & expected[w] == expected[w]
        });
        if done {
            break;
        }
        spins += 1;
        if spins >= SHOOTDOWN_WATCHDOG_SPINS {
            let acked0 = SHOOTDOWN_ACK_MASK.get(me)[0].load(Ordering::SeqCst);
            let missing0 = expected[0] & !acked0;
            panic!(
                "TLB shootdown WEDGE: core {} spun {} iters waiting acks (addr={:#x}); \
                 expected(w0)={:#x} acked(w0)={:#x} MISSING-ACK cores(w0)={:#x}",
                me, spins, addr, expected[0], acked0, missing0
            );
        }
        core::hint::spin_loop();
    }
}

/// Broadcast a single-page TLB shootdown to all other cores and wait for acks (§10.5).
///
/// # Safety
/// Interrupts should be disabled on the calling core before this call.
pub unsafe fn broadcast_tlb_shootdown(virt_addr: u64) {
    // SAFETY: caller holds IF=0; per-core request slots make concurrency safe.
    unsafe { request_and_wait(virt_addr); }
}

/// IPI handler - invoked from the IDT stub (`ipi_dispatch`) on the receiving core.
///
/// # Safety
/// Called from raw interrupt context with interrupts disabled.
pub unsafe fn ipi_handler(vector: u8) {
    match vector {
        vectors::WAKE_RECEIVER => {
            // ipi_wake_stub now calls timer_tick_from_irq directly (with the
            // same swapgs protocol as timer_isr_stub) so it never reaches this
            // branch.  Left as a no-op for robustness if another path routes
            // here unexpectedly.
        }
        vectors::TLB_SHOOTDOWN => handle_tlb_shootdown(),
        vectors::SCHEDULER_TICK => {
            // Same reasoning: just let the loop pick up new work after iretq.
        }
        _ => {}
    }
    // Send EOI to allow the next interrupt to be delivered.
    // SAFETY: called from interrupt context; APIC is mapped.
    unsafe { crate::arch::x86_64::boot::apic_send_eoi() }
}

/// Broadcast a full TLB flush to all other cores and flush locally (§10.5).
///
/// Used when an entire address space is torn down (task death) so that all
/// non-global TLB entries are invalidated on every core before the backing
/// frames are returned to the allocator.
///
/// Saves and restores the caller's interrupt flag, so this may be called
/// with interrupts either enabled or disabled.
///
/// # Safety
/// The local APIC must be initialised.
pub unsafe fn broadcast_full_tlb_flush() {
    // Save the interrupt flag and disable interrupts for the shootdown protocol.
    // SAFETY: pushfq/cli are always valid in ring 0.
    let rflags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nostack));
        core::arch::asm!("cli", options(nostack, nomem));
    }

    // Flush locally (CR3 reload invalidates all non-global TLB entries on this core).
    invalidate(!0u64);

    // Broadcast a full-flush request (addr = !0) to every other core via the per-core path; remote
    // handlers reload CR3 on the `!0` sentinel rather than invlpg one page.
    // SAFETY: APIC mapped; IF=0 (just disabled above).
    unsafe { request_and_wait(!0u64); }

    // Restore the caller's interrupt flag.
    // SAFETY: push/popfq restores exactly the flags in effect on entry.
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) rflags, options(nostack));
    }
}

fn handle_tlb_shootdown() {
    // Service every other core's pending request (invalidate + ack). One delivery may coalesce
    // several initiators (the APIC IRR holds one bit per vector); the scan acks each of them once.
    service_pending(this_core());
}

#[inline]
unsafe fn read_apic_reg(addr: u64) -> u32 {
    // SAFETY: addr is a valid MMIO register inside the mapped APIC page.
    unsafe { (addr as *const u32).read_volatile() }
}

#[inline]
unsafe fn write_apic_reg(addr: u64, val: u32) {
    // SAFETY: addr is a valid MMIO register inside the mapped APIC page.
    unsafe { (addr as *mut u32).write_volatile(val) }
}
