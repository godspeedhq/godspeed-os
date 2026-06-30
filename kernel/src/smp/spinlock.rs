// SPDX-License-Identifier: GPL-2.0-only
//! `SpinLock<T>` - interior-mutable spinlock for SMP-safe global state.
//!
//! The `unsafe impl Sync/Send` lives here (permitted layer: smp/).
//! All call sites using `lock()` / `try_lock()` are unsafe-free.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
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
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(val) }
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
    /// thereof). The zeroed `locked: AtomicBool` is `false` (unlocked), which
    /// is the correct initial lock state.
    pub const ZEROED: Self = unsafe { core::mem::zeroed() };

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self.locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
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
        let was_enabled = unsafe {
            let rflags: u64;
            core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nostack));
            core::arch::asm!("cli", options(nomem, nostack));
            (rflags & (1 << 9)) != 0
        };
        while self.locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self, irq_restore: was_enabled }
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
    let was_enabled = unsafe {
        let rflags: u64;
        core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nostack));
        core::arch::asm!("cli", options(nomem, nostack));
        (rflags & (1 << 9)) != 0
    };
    let r = f();
    if was_enabled {
        // SAFETY: restore the prior (enabled) interrupt state we masked above.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
    }
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
        self.lock.locked.store(false, Ordering::Release);
        if self.irq_restore {
            // SAFETY: restore the prior (enabled) interrupt state that lock_irq masked.
            unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
        }
    }
}
