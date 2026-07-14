// SPDX-License-Identifier: GPL-2.0-only
//! Inter-Processor Interrupts - §9.4, §8.4, §10.5.
//!
//! Three distinct purposes:
//!   1. Wake a task blocked on `recv` after a cross-core `send` enqueues.
//!   2. TLB shootdown after a page is unmapped (§10.5).
//!   3. Cross-core scheduler preemption (timer overflow).

use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use crate::smp::percpu::{PerCore, num_cores};

/// Vector numbers for each IPI purpose.
pub mod vectors {
    pub const WAKE_RECEIVER:  u8 = 0xF0;
    pub const TLB_SHOOTDOWN:  u8 = 0xF1;
    pub const SCHEDULER_TICK: u8 = 0xF2;
}


// Per-core TLB shootdown state - one independent request slot per INITIATING core, so concurrent
// shootdowns never share a counter or address. The single-global `TLB_ACK` / `TLB_SHOOTDOWN_ADDR`
// this replaced DEADLOCKED when two cores unmapped at once: each spun IF=0 waiting for the other's
// ack while being a target of the other's all-excluding-self broadcast, so neither could ack the
// other. Now a core waiting for its own acks ALSO services every other core's pending request
// (`service_pending`), so concurrent shootdowns serialize cleanly instead of deadlocking. This was the
// max-carnage wedge at 71K rounds: heavy concurrent reclaims across cores (§10.5).
//
// All three are boot-allocated arenas (§26.6.1) sized to the cores Limine actually reported - NO fixed
// ceiling, so the machine's real core count is the only bound (RAM caps it: the carve panics loudly if
// it cannot be backed). A 4-core box reserves 4 slots; a 512-core box reserves 512.
//
//   SHOOTDOWN_ADDR.get(x)  - the VA core x is invalidating (`!0` = full CR3 flush).
//   ack_word(x, w) / exp_word(x, w) - two FLAT `num_cores * words_per_core` bitmask arenas (one bit per
//                            (initiator, receiver) pair, `words_per_core = ceil(num_cores/64)`), indexed
//                            `[x * words_per_core + w]`. In ACK, bit y set = core y has serviced x's
//                            CURRENT request; the initiator CLEARS its words to publish (a clear bit =
//                            "service me"), each servicer sets its own bit, and the initiator waits
//                            until every EXPECTED bit is set. ACK starts all-set (!0) so a never-
//                            requested slot is not spuriously serviced; EXPECTED is (re)computed per
//                            request. Flat arenas because the per-initiator WIDTH scales with the core
//                            count - no fixed `[_; K]` can size it dynamically.

/// Watchdog bound for the shootdown ack-wait spin (see `request_and_wait`). A real shootdown completes
/// in a few thousand iterations even under contention; ~5x10^8 is thousands of times that, so it fires
/// ONLY on a true wedge (a core that will never ack) and turns a silent freeze into a loud, pinpointed
/// panic. At ~1.5-2 GHz this is on the order of a few seconds of spin before the panic.
const SHOOTDOWN_WATCHDOG_SPINS: u64 = 500_000_000;

static SHOOTDOWN_ADDR: PerCore<AtomicU64> = PerCore::new();

/// u64 words per initiator: `ceil(num_cores/64)`. Set once at boot in `init_arenas`.
static WORDS_PER_CORE: AtomicUsize = AtomicUsize::new(1);
/// Base of the flat `num_cores * words_per_core` ACK bitmask arena (each element an `AtomicU64`).
static ACK_MASK: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());
/// Base of the flat `num_cores * words_per_core` EXPECTED bitmask arena (the acks each initiator awaits).
static EXPECTED: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

#[inline]
fn words_per_core() -> usize {
    WORDS_PER_CORE.load(Ordering::Acquire)
}

/// The ACK bitmask word `(initiator, word)` in the flat arena.
#[inline]
fn ack_word(initiator: usize, word: usize) -> &'static AtomicU64 {
    let base = ACK_MASK.load(Ordering::Acquire);
    // SAFETY: base points to `num_cores * words_per_core` initialised, never-freed `AtomicU64`s
    // (init_arenas); `initiator < num_cores()` and `word < words_per_core()` by every caller's loop
    // bounds, so the index is in range. `AtomicU64: Sync` makes the shared ref sound across cores.
    unsafe { &*base.add(initiator * words_per_core() + word) }
}

/// The EXPECTED bitmask word `(initiator, word)` in the flat arena.
#[inline]
fn exp_word(initiator: usize, word: usize) -> &'static AtomicU64 {
    let base = EXPECTED.load(Ordering::Acquire);
    // SAFETY: as `ack_word` - in-range index into the EXPECTED flat arena of the same shape.
    unsafe { &*base.add(initiator * words_per_core() + word) }
}

/// Allocate the shootdown arenas for `n` cores. Called once at boot (`smp::percpu_init`), after the
/// frame allocator is up and before any shootdown can run (before APs / spawn). ADDR starts 0; every
/// ACK word starts all-set so a never-requested slot reads "already acked by everyone"; EXPECTED starts
/// 0 (recomputed per request).
pub fn init_arenas(n: usize) {
    let wpc = n.div_ceil(64);
    WORDS_PER_CORE.store(wpc, Ordering::Release);
    SHOOTDOWN_ADDR.init_with(n, |_| AtomicU64::new(0));
    let ack = crate::smp::percpu::alloc_atomic_u64_slice(n * wpc, !0);
    let exp = crate::smp::percpu::alloc_atomic_u64_slice(n * wpc, 0);
    ACK_MASK.store(ack.as_ptr() as *mut AtomicU64, Ordering::Release);
    EXPECTED.store(exp.as_ptr() as *mut AtomicU64, Ordering::Release);
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
    let lapic = unsafe { crate::arch::imp::boot::get_lapic_id() };
    crate::smp::core::lapic_to_core_id(lapic) as usize
}

/// Invalidate `addr` on THIS core - a single-page `invlpg`, or a full CR3 reload for the `!0` sentinel.
#[inline]
fn invalidate(addr: u64) {
    if addr == !0u64 {
        // SAFETY: reloading the page-table base with the same value is always valid in ring 0.
        let base = crate::arch::imp::read_page_table_base();
        unsafe { crate::arch::imp::write_page_table_base(base); }
    } else {
        // SAFETY: single-page local TLB invalidation; `addr` is a virtual address.
        unsafe { crate::arch::imp::invalidate_tlb_page(addr); }
    }
}

/// Publish, into `me`'s EXPECTED words, the set of acks it must collect: a bit for every ready core
/// except `me`. Returns nothing - the spin reads the EXPECTED arena directly (its width is dynamic, so
/// it cannot be a fixed stack array). Call after clearing `me`'s EXPECTED words is not needed: each word
/// is overwritten wholesale here.
fn publish_expected(me: usize) {
    let wpc = words_per_core();
    // Zero every word first (a core that became not-ready since last time must not linger set).
    for w in 0..wpc {
        exp_word(me, w).store(0, Ordering::SeqCst);
    }
    for x in 0..num_cores() {
        if x != me && crate::smp::core::is_ready(x as u32) {
            let (w, b) = word_bit(x);
            exp_word(me, w).fetch_or(b, Ordering::SeqCst);
        }
    }
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
        if ack_word(x, mw).load(Ordering::SeqCst) & mb == 0 {
            invalidate(SHOOTDOWN_ADDR.get(x).load(Ordering::SeqCst));
            ack_word(x, mw).fetch_or(mb, Ordering::SeqCst);
        }
    }
}

/// Send a fixed-delivery IPI to a specific core.
///
/// # Safety
/// `core_id` must refer to a ready core; the local APIC must be mapped.
pub unsafe fn send_ipi(core_id: u32, vector: u8) {
    // Resolve the core -> LAPIC mapping (smp owns it) and hand the raw send to the arch seam (arch owns
    // the APIC MMIO). The protocol logic below stays here; only the register programming lives in arch.
    let lapic_id = crate::smp::core::core_lapic_id(core_id);
    // SAFETY: local APIC mapped after init_local_apic; lapic_id is a ready core's id.
    unsafe { crate::arch::imp::boot::send_ipi_to_lapic(lapic_id, vector); }
}

/// Broadcast a `TLB_SHOOTDOWN` IPI to all OTHER cores (all-excluding-self shorthand).
///
/// # Safety
/// The APIC must be mapped; the caller holds IF=0 (or has saved/disabled it).
unsafe fn broadcast_shootdown_ipi() {
    // smp owns WHICH vector (the TLB_SHOOTDOWN kind); arch owns the APIC broadcast MMIO.
    // SAFETY: APIC mapped; caller holds IF=0.
    unsafe { crate::arch::imp::boot::broadcast_ipi_all_but_self(vectors::TLB_SHOOTDOWN); }
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
    let wpc = words_per_core();
    // The acks we must collect: a bit for every OTHER ready core, published into me's EXPECTED words.
    publish_expected(me);

    // Publish: set the address, THEN clear every ack-mask word. The clear IS the "new request" signal
    // (a servicer acts on a clear bit), so the address must land first - a servicer seeing its bit
    // clear then reads the new address.
    SHOOTDOWN_ADDR.get(me).store(addr, Ordering::SeqCst);
    for w in 0..wpc {
        ack_word(me, w).store(0, Ordering::SeqCst);
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
        let done = (0..wpc).all(|w| {
            let acked = ack_word(me, w).load(Ordering::SeqCst);
            let exp = exp_word(me, w).load(Ordering::SeqCst);
            acked & exp == exp
        });
        if done {
            break;
        }
        spins += 1;
        if spins >= SHOOTDOWN_WATCHDOG_SPINS {
            let exp0 = exp_word(me, 0).load(Ordering::SeqCst);
            let acked0 = ack_word(me, 0).load(Ordering::SeqCst);
            let missing0 = exp0 & !acked0;
            panic!(
                "TLB shootdown WEDGE: core {} spun {} iters waiting acks (addr={:#x}); \
                 expected(w0)={:#x} acked(w0)={:#x} MISSING-ACK cores(w0)={:#x}",
                me, spins, addr, exp0, acked0, missing0
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
    unsafe { crate::arch::imp::boot::apic_send_eoi() }
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
    let was_enabled = crate::arch::imp::local_irq_save();

    // Flush locally (CR3 reload invalidates all non-global TLB entries on this core).
    invalidate(!0u64);

    // Broadcast a full-flush request (addr = !0) to every other core via the per-core path; remote
    // handlers reload CR3 on the `!0` sentinel rather than invlpg one page.
    // SAFETY: APIC mapped; IF=0 (just disabled above).
    unsafe { request_and_wait(!0u64); }

    // Restore the caller's interrupt flag.
    crate::arch::imp::local_irq_restore(was_enabled);
}

fn handle_tlb_shootdown() {
    // Service every other core's pending request (invalidate + ack). One delivery may coalesce
    // several initiators (the APIC IRR holds one bit per vector); the scan acks each of them once.
    service_pending(this_core());
}

