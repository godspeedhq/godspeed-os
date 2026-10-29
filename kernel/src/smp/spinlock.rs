// SPDX-License-Identifier: GPL-2.0-only
//! `SpinLock<T>` - interior-mutable spinlock for SMP-safe global state.
//!
//! The `unsafe impl Sync/Send` lives here (permitted layer: smp/).
//! All call sites using `lock()` / `try_lock()` are unsafe-free.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Local-core interrupt masking for the irq-safe lock (`lock_irq` / `without_interrupts`), routed through
// the arch seam (`arch::imp`) so the arch-specific asm lives ONLY in arch/ (aarch64 Phase 0). The
// pure-logic host/lib build provides a no-op `arch::imp` stub (lib.rs), so SpinLock stays
// host-unit-testable exactly as lib.rs promises - no hardware asm in this neutral file.
#[inline(always)]
fn irq_save() -> bool { crate::arch::imp::local_irq_save() }
#[inline(always)]
fn irq_restore(was_enabled: bool) { crate::arch::imp::local_irq_restore(was_enabled) }

/// Spins between two readings of the clock while waiting, so the uncontended acquire never reads it and a
/// contended one reads it rarely.
const SPINS_PER_LOOK: u32 = 4096;

/// Watchdog windows a waiter will open when the lock CHANGED HANDS inside the last one: contended, so
/// late, not stuck. Bounded, so a lock that is passed round forever while this waiter never wins still
/// ends loudly rather than in the unbounded spin this replaced (26.6).
const MAX_WINDOWS: u32 = 4;

/// Where no clock bound exists (an arch whose watchdog figure is `0`, or before calibration), the old
/// spin count stands in: a few seconds at GHz, a huge margin over any real hold.
const LOCK_WATCHDOG_SPINS: u64 = 1_000_000_000;

/// The wait for a held lock, bounded by TIME and told apart from contention (`backlog/49`).
///
/// Each lock keeps, beside its held flag, a count of its RELEASES that only the holder writes (a plain
/// load and store just before the release, so the acquire is the single compare-and-swap it always was).
/// A waiter that sees the count change while it spins knows the lock is changing hands - contended, not
/// stuck - which a held flag alone cannot say.
///
/// Until 2026-10-28 every SpinLock and the frame allocator's lock panicked after a billion SPINS: a count,
/// so a different wall time on every machine, and blind to whether the lock was stuck or merely busy. Now
/// a window is the liveness watchdog's own allowance, read from the arch in counter ticks, and at its end
/// the waiter asks one question - did the release count change? If it did, the lock is being taken and released by
/// others, and the waiter opens another window (at most `MAX_WINDOWS`). If the SAME hold lasted the whole
/// window, no release happened in seconds, which is the deadlock or the dead holder this guard exists for,
/// and it panics saying how long, and which lock.
pub struct Wait {
    spins: u64,
    looks: u32,
    windows: u32,
    t0: u64,
    releases0: u32,
}

impl Wait {
    pub const fn new() -> Self { Self { spins: 0, looks: 0, windows: 0, t0: 0, releases0: 0 } }

    /// Called on each failed acquire with the lock's release count. Returns only if the wait may go on.
    #[inline]
    pub fn spun(&mut self, what: &'static str, addr: usize, releases: &AtomicU32) {
        self.spins += 1;
        self.looks += 1;
        if self.looks < SPINS_PER_LOOK { return; }
        self.looks = 0;
        self.look(what, addr, releases.load(Ordering::Relaxed));
    }

    #[cold]
    #[inline(never)]
    fn look(&mut self, what: &'static str, addr: usize, releases: u32) {
        let bound = crate::arch::imp::liveness_deadline_cycles();
        let now = crate::arch::imp::read_cycle_counter();
        if let Some(held_for) = self.decide(bound, now, releases) {
            wedge(what, addr, self.spins, held_for, bound, self.windows);
        }
    }

    /// The decision alone, with the clock passed in so the host tests can drive it: `Some(held_for)`
    /// when the lock has been held by ONE holder past the bound (or, with no bound, past the spin count),
    /// `None` while the wait may go on.
    fn decide(&mut self, bound: u64, now: u64, releases: u32) -> Option<u64> {
        if bound == 0 {
            return if self.spins >= LOCK_WATCHDOG_SPINS { Some(0) } else { None };
        }
        if self.t0 == 0 {
            self.t0 = now;
            self.releases0 = releases;
            return None;
        }
        let held_for = now.wrapping_sub(self.t0);
        if held_for <= bound { return None; }
        if releases != self.releases0 && self.windows < MAX_WINDOWS {
            // Changed hands: contended. A new window, measured from here.
            self.windows += 1;
            self.t0 = now;
            self.releases0 = releases;
            return None;
        }
        Some(held_for)
    }
}

#[cfg(test)]
mod wait_tests {
    use super::{Wait, MAX_WINDOWS};

    // A window of 1000 ticks, opened at tick 1000.
    fn opened() -> Wait {
        let mut w = Wait::new();
        assert_eq!(w.decide(1000, 1000, 7), None);
        w
    }

    #[test]
    fn a_hold_that_never_releases_is_a_wedge_once_past_the_bound() {
        let mut w = opened();
        assert_eq!(w.decide(1000, 1900, 7), None, "inside the window: wait");
        assert_eq!(w.decide(1000, 2001, 7), Some(1001), "same hold past the bound: wedge");
    }

    #[test]
    fn a_lock_changing_hands_is_contention_and_gets_more_windows() {
        let mut w = opened();
        let mut now = 1000;
        for n in 0..MAX_WINDOWS {
            now += 1001;
            assert_eq!(w.decide(1000, now, 8 + n), None, "released in window {}: wait on", n);
        }
        // Still changing hands, but the windows are spent: bounded, so it ends loudly (26.6).
        now += 1001;
        assert!(w.decide(1000, now, 100).is_some(), "past MAX_WINDOWS: wedge");
    }

    #[test]
    fn contention_then_a_stuck_holder_is_still_caught() {
        let mut w = opened();
        assert_eq!(w.decide(1000, 2001, 8), None, "changed hands: another window");
        assert_eq!(w.decide(1000, 3002, 8), Some(1001), "then no release for a whole window: wedge");
    }

    #[test]
    fn with_no_clock_bound_the_spin_count_decides() {
        let mut w = Wait::new();
        assert_eq!(w.decide(0, 0, 0), None);
        w.spins = super::LOCK_WATCHDOG_SPINS;
        assert_eq!(w.decide(0, 0, 0), Some(0));
    }
}

/// Out-of-line, non-generic panic for the lock watchdog - keeps the hot path lean (the fast, uncontended
/// acquire pays nothing; only a wedged spinner ever reaches here). `addr` identifies WHICH static lock
/// (match it against the kernel symbol map / the static addresses in the boot log).
#[cold]
#[inline(never)]
fn wedge(what: &'static str, addr: usize, spins: u64, held_for: u64, bound: u64, windows: u32) -> ! {
    if bound == 0 {
        panic!("{} WEDGE: lock@{:#x} not released after {} spins (this arch gives the watchdog no clock bound)",
               what, addr, spins);
    }
    panic!("{} WEDGE: lock@{:#x} held by ONE holder for {} counter ticks ({}x the watchdog's {}) with no release - \
            a deadlock or a dead holder, not contention ({} earlier window(s) saw it change hands)",
           what, addr, held_for, held_for / bound, bound, windows);
}

pub struct SpinLock<T> {
    locked:   AtomicBool,
    /// How many times this lock has been released (wrapping). Written only by the holder; read by a
    /// waiter to tell contention from a holder that never lets go (`Wait`).
    releases: AtomicU32,
    data:   UnsafeCell<T>,
}

// SAFETY: SpinLock<T> serialises all access to T via an atomic spinlock; T: Send suffices.
unsafe impl<T: Send> Send for SpinLock<T> {}
// SAFETY: SpinLock<T> is safe to share across cores; the lock ensures exclusive access.
unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// True only for a guard from `lock_irq`: interrupts were enabled and were masked for the hold,
    /// so they must be re-enabled on drop. `lock`/`try_lock` leave this false (they don't touch IF).
    irq_restore: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(val: T) -> Self {
        Self { locked: AtomicBool::new(false), releases: AtomicU32::new(0), data: UnsafeCell::new(val) }
    }

    /// All-zeroes initializer for placing a large `SpinLock<T>` in `.bss`.
    ///
    /// `SpinLock::new([E; N])` materialises the value with undef padding bytes,
    /// which LLD rejects when the symbol is placed in `.bss`; an all-zeroes
    /// value has no undef bytes. Limine zeroes `.bss` before kernel entry, so
    /// the runtime bit pattern matches this const.
    ///
    /// SAFETY: the all-zeroes bit pattern must be a valid `T`. This is the
    /// caller's responsibility via the `T` they instantiate: only reference
    /// `ZEROED` when every field of `T` is valid at zero (integers, `bool`
    /// false, `AtomicBool(false)`, `Option` `None` at discriminant 0, arrays
    /// thereof). The zeroed `locked` is `false` (unlocked) and the zeroed
    /// release count is 0, which is the correct initial lock state.
    pub const ZEROED: Self = unsafe { core::mem::zeroed() };

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        self.acquire();
        SpinLockGuard { lock: self, irq_restore: false }
    }

    /// Interrupt-safe acquire: mask interrupts for the whole hold (restored on guard drop). REQUIRED
    /// for a lock ALSO taken in interrupt context on the same core (the §contract in
    /// `without_interrupts`) - without it a task preempted mid-hold leaves the lock held by an
    /// un-reschedulable holder, which then deadlocks the supervisor respawn (it pins Core 0). All
    /// acquisitions of such a lock must use `lock_irq`, so no holder is ever preemptible. Nests
    /// correctly: a nested acquire captures IF=0 and skips the re-enable.
    #[inline]
    pub fn lock_irq(&self) -> SpinLockGuard<'_, T> {
        // SAFETY: reading RFLAGS + cli are local-core, no memory effects; the prior IF (bit 9) is
        // captured and restored exactly on drop. Disable BEFORE spinning so the hold is never preempted.
        let was_enabled = irq_save();
        self.acquire();
        SpinLockGuard { lock: self, irq_restore: was_enabled }
    }

    /// Take the lock, waiting (bounded, `Wait`) while another holds it.
    #[inline]
    fn acquire(&self) {
        let mut wait = Wait::new();
        while self.locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            wait.spun("SpinLock", self as *const _ as usize, &self.releases);
            core::hint::spin_loop();
        }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinLockGuard { lock: self, irq_restore: false })
        } else {
            None
        }
    }
}

/// Run `f` with interrupts disabled on the local core, restoring the prior interrupt state afterward.
///
/// REQUIRED when acquiring a `SpinLock` that is ALSO taken in interrupt context on the same core -
/// e.g. `KSTACK_USED`, held by `alloc_kstack`/`free_kstack` in the syscall spawn/kill paths AND by
/// `drain_pending_kstack` from the timer ISR. Without masking, a timer firing mid-critical-section
/// re-enters the same lock in the ISR on that very core and **self-deadlocks** (the lock is never
/// released → the whole machine freezes - observed once per ~60k kills under `chaos max-carnage`).
/// The protected sections are short, so the interrupts-off window is negligible. Nests correctly: an
/// inner call captures IF=0 and skips the re-enable, so the outermost restorer owns the re-enable.
#[inline]
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: reading RFLAGS and toggling IF are local-core operations with no memory effects; the
    // prior IF (bit 9) is captured and restored exactly. `pushfq; pop` is balanced (nostack matches
    // the existing convention in smp/ipi.rs; the kernel target has no red zone).
    let was_enabled = irq_save();
    let r = f();
    irq_restore(was_enabled);
    r
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: lock is held; no other reference to data exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: lock is held; no other mutable reference to data exists.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Only the holder writes the count, so a plain read and store is exact; the Release below
        // publishes it with the data.
        let n = self.lock.releases.load(Ordering::Relaxed);
        self.lock.releases.store(n.wrapping_add(1), Ordering::Relaxed);
        self.lock.locked.store(false, Ordering::Release);
        irq_restore(self.irq_restore);
    }
}
