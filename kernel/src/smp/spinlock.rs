//! `SpinLock<T>` — interior-mutable spinlock for SMP-safe global state.
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
}

impl<T> SpinLock<T> {
    pub const fn new(val: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(val) }
    }

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self.locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }
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
    }
}
