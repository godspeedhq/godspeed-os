// SPDX-License-Identifier: GPL-2.0-only
//! `NameTable` - a bounded, single-writer / many-reader table of short names.
//!
//! ## Why this lives in `smp/`, and not where it is used
//!
//! Task names used to be `[&'static str; MAX_TASKS]`, which quietly required every name to be a
//! string literal compiled into the kernel - and that is the reason the kernel held a
//! `service_config` row per service: a caller-supplied name had nowhere to live. Owning the bytes is
//! what lets a SPAWNER name what it spawns (`docs/probe-params-design.md`).
//!
//! Owning them needs interior mutability behind a shared `static`, which is `unsafe`. Section 18.5
//! says new `unsafe` must first try to live in a permitted layer rather than grow a grandfathered
//! file - and this is, precisely, a concurrency primitive: a shared array with one writer per slot
//! and readers on every core. It belongs beside `SpinLock`, audited once, rather than as raw-pointer
//! code open-coded in `task/`.
//!
//! ## The discipline that makes it sound
//!
//! - **One writer per slot.** `set` is called from the spawn path with the slot reserved and
//!   interrupts off, before the task is enqueued and therefore before any other core can see it.
//! - **Length last, and length first.** The writer publishes the bytes, then the length with
//!   `Release`; a reader loads the length with `Acquire` and only then the bytes below it. A reader
//!   that races a write sees the old length over new bytes, or the new length over new bytes - never
//!   a length that outruns what has been written.
//! - **Bounded and flat (26.6):** a fixed `N * L` byte array in `.bss`. No heap, no interner, no
//!   lifetimes to reason about.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

/// `N` slots of at most `L` bytes each, stored ATOMICALLY - so every operation is safe.
///
/// The sibling of `NameTable`, for the case that does not need to BORROW a name: comparison only.
/// `NameTable` has to hand back a `&'static str`, which forces a raw read out of an `UnsafeCell`;
/// where a caller only asks "is this slot this name?", atomic bytes answer it with no `unsafe` at
/// all. That matters beyond tidiness: the caller here is `task/scheduler.rs`, one of 18.5's
/// GRANDFATHERED floors, which may not grow its unsafe count without a CLAUDE.md amendment. Choosing
/// the representation that needs none is cheaper than the amendment, and better.
///
/// Per-byte atomics are a real cost, paid once per spawn on a table read only by `AcquireSendCap`.
pub struct AtomicNameSet<const N: usize, const L: usize> {
    bytes: [[core::sync::atomic::AtomicU8; L]; N],
    lens:  [AtomicU8; N],
}

impl<const N: usize, const L: usize> AtomicNameSet<N, L> {
    pub const fn new() -> Self {
        Self {
            bytes: [const { [const { core::sync::atomic::AtomicU8::new(0) }; L] }; N],
            lens:  [const { AtomicU8::new(0) }; N],
        }
    }

    /// Record `name` in `slot`, truncating at `L`. Safe: every write is atomic, so concurrent
    /// writers produce a wrong name rather than undefined behaviour - and there is only ever one.
    pub fn set(&self, slot: usize, name: &str) {
        if slot >= N { return; }
        let b = name.as_bytes();
        let n = b.len().min(L);
        for i in 0..L {
            self.bytes[slot][i].store(if i < n { b[i] } else { 0 }, Ordering::Relaxed);
        }
        // LENGTH LAST, as in `NameTable`: it publishes the bytes above.
        self.lens[slot].store(n as u8, Ordering::Release);
    }

    /// Is `slot` exactly `name`?
    pub fn matches(&self, slot: usize, name: &str) -> bool {
        if slot >= N { return false; }
        let b = name.as_bytes();
        let n = self.lens[slot].load(Ordering::Acquire) as usize;
        if n != b.len() || n > L { return false; }
        (0..n).all(|i| self.bytes[slot][i].load(Ordering::Relaxed) == b[i])
    }
}

/// `N` slots of at most `L` bytes each.
pub struct NameTable<const N: usize, const L: usize> {
    bytes: UnsafeCell<[[u8; L]; N]>,
    lens:  [AtomicU8; N],
}

// SAFETY: every write goes through `set`, which the caller guarantees is the only writer for that
// slot (see the module discipline above); readers only ever read bytes below an `Acquire`-loaded
// length that a `Release` store published after those bytes were written.
unsafe impl<const N: usize, const L: usize> Sync for NameTable<N, L> {}

impl<const N: usize, const L: usize> NameTable<N, L> {
    pub const fn new() -> Self {
        Self {
            bytes: UnsafeCell::new([[0u8; L]; N]),
            lens:  [const { AtomicU8::new(0) }; N],
        }
    }

    /// This slot's name, or `""` for an out-of-range slot or one never set.
    ///
    /// The borrow is genuinely `'static`: the table is a kernel-lifetime `static`, and the bytes of
    /// a slot are only ever rewritten by the single writer that owns it.
    pub fn get(&'static self, slot: usize) -> &'static str {
        if slot >= N { return ""; }
        let len = self.lens[slot].load(Ordering::Acquire) as usize;
        // SAFETY: `len` was published with `Release` AFTER the bytes below it were written, so those
        // bytes are initialised and stable. The pointer targets a `static`, so the `'static` borrow
        // outlives every caller. No `&mut` to this slot can exist concurrently (single writer, and
        // it runs with the slot reserved and interrupts off).
        let b = unsafe {
            let rows: &[[u8; L]; N] = &*self.bytes.get();
            &rows[slot][..len.min(L)]
        };
        core::str::from_utf8(b).unwrap_or("")
    }

    /// Record this slot's name, truncating at `L` bytes rather than refusing - a name is an identity,
    /// and a spawn should not fail because someone chose a long one.
    ///
    /// # Safety
    /// The caller must be the only writer for `slot` for the duration of this call. In the kernel
    /// that is the spawn path, which holds the slot reserved with interrupts off.
    pub unsafe fn set(&self, slot: usize, name: &str) {
        if slot >= N { return; }
        let b = name.as_bytes();
        let n = b.len().min(L);
        // SAFETY: the caller guarantees exclusive write access to this slot; no reader can observe
        // the bytes as part of the name until the length store below publishes them.
        unsafe {
            let rows: &mut [[u8; L]; N] = &mut *self.bytes.get();
            rows[slot] = [0u8; L];
            rows[slot][..n].copy_from_slice(&b[..n]);
        }
        // LENGTH LAST: this is the release that publishes the bytes above.
        self.lens[slot].store(n as u8, Ordering::Release);
    }
}
