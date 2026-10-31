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
//! It is a concurrency primitive - a shared array with one writer per slot and readers on every core
//! - so it belongs beside `SpinLock`, not open-coded in `task/`.
//!
//! ## The discipline that makes it sound
//!
//! - **Atomic bytes, so nothing here is `unsafe`.** A read racing a write is a wrong answer, never
//!   undefined behaviour.
//! - **A read is a COPY, never a borrow** (backlog/90 D24). `get` used to hand out `&'static str`
//!   into a slot's bytes, and a slot is REUSED: the next task to take it rewrote those bytes under
//!   whoever still held the borrow - a name that changed in the hand of the code printing it. Now
//!   `get` returns a `Name`, the bytes copied out.
//! - **A copy is whole or retried.** Each slot has a sequence counter, odd while `set` is writing.
//!   A reader copies between two reads of it and keeps the copy only if both match and are even, so it
//!   never mixes two lives' bytes. One writer per slot (the spawn path, slot reserved, interrupts off)
//!   keeps a rewrite brief; a reader that keeps losing gives up and answers `""` rather than spin.
//! - **Bounded and flat (26.6):** fixed `N * L` atomic bytes in `.bss`. No heap, no interner.

use core::sync::atomic::{fence, AtomicU32, AtomicU8, Ordering};

/// `N` slots of at most `L` bytes each, stored ATOMICALLY - so every operation is safe.
///
/// The sibling of `NameTable`, for the case that only ever COMPARES a name. Both are atomic now;
/// this one has no sequence counter because a comparison racing a rewrite answers wrongly once, and
/// its one caller (`AcquireSendCap`) can afford that where a printed or reported name could not.
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

/// A name copied out of a `NameTable` slot: at most `L` bytes, owned, so it cannot change under its
/// holder when the slot is reused (backlog/90 D24). Reads as a `&str` (`Deref`, `Display`, `==`).
#[derive(Clone, Copy)]
pub struct Name<const L: usize> {
    buf: [u8; L],
    len: usize,
}

impl<const L: usize> Name<L> {
    /// The empty name, as a slot never set reads.
    pub const fn empty() -> Self {
        Self { buf: [0u8; L], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const L: usize> core::ops::Deref for Name<L> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const L: usize> core::fmt::Display for Name<L> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const L: usize> core::fmt::Debug for Name<L> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const L: usize> PartialEq<str> for Name<L> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const L: usize> PartialEq<&str> for Name<L> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// `N` slots of at most `L` bytes each, one writer per slot, read by copy (`Name`).
pub struct NameTable<const N: usize, const L: usize> {
    bytes: [[AtomicU8; L]; N],
    lens:  [AtomicU8; N],
    /// Odd while `set` is writing the slot; a copy is kept only if it reads the same even value on
    /// both sides of the copy.
    seq:   [AtomicU32; N],
}

/// How many times `get` tries for a whole copy before answering `""`. A rewrite is a few hundred
/// stores with interrupts off, so a reader that loses this many times in a row is not going to win.
const GET_TRIES: usize = 8;

impl<const N: usize, const L: usize> NameTable<N, L> {
    pub const fn new() -> Self {
        Self {
            bytes: [const { [const { AtomicU8::new(0) }; L] }; N],
            lens:  [const { AtomicU8::new(0) }; N],
            seq:   [const { AtomicU32::new(0) }; N],
        }
    }

    /// A copy of this slot's name, or the empty name for an out-of-range slot, one never set, or one
    /// being rewritten for longer than `GET_TRIES` reads.
    pub fn get(&self, slot: usize) -> Name<L> {
        if slot >= N {
            return Name::empty();
        }
        for _ in 0..GET_TRIES {
            let before = self.seq[slot].load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let mut name = Name::empty();
            name.len = (self.lens[slot].load(Ordering::Relaxed) as usize).min(L);
            for i in 0..name.len {
                name.buf[i] = self.bytes[slot][i].load(Ordering::Relaxed);
            }
            // The copy's loads may not be reordered after the second read of the counter.
            fence(Ordering::Acquire);
            if self.seq[slot].load(Ordering::Relaxed) == before {
                return name;
            }
        }
        Name::empty()
    }

    /// Record this slot's name, truncating at `L` bytes rather than refusing - a name is an identity,
    /// and a spawn should not fail because someone chose a long one.
    ///
    /// The caller must be the only writer for `slot` for the duration of this call - the spawn path,
    /// which holds the slot reserved with interrupts off. A second writer would give a wrong name, not
    /// undefined behaviour: every store is atomic.
    pub fn set(&self, slot: usize, name: &str) {
        if slot >= N {
            return;
        }
        let b = name.as_bytes();
        let n = b.len().min(L);
        // Odd: a write is in progress, so a reader's copy across it is thrown away.
        self.seq[slot].fetch_add(1, Ordering::Relaxed);
        // The stores below may not be reordered before the counter went odd.
        fence(Ordering::Release);
        for i in 0..L {
            self.bytes[slot][i].store(if i < n { b[i] } else { 0 }, Ordering::Relaxed);
        }
        self.lens[slot].store(n as u8, Ordering::Relaxed);
        // Even again: everything above is published by this release.
        self.seq[slot].fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_survives_the_slot_being_reused() {
        // backlog/90 D24: a name borrowed out of a slot changed when the slot was reused.
        let t: NameTable<4, 16> = NameTable::new();
        t.set(1, "fs");
        let held = t.get(1);
        t.set(1, "shell");
        assert_eq!(held, "fs");
        assert_eq!(t.get(1), "shell");
    }

    #[test]
    fn truncates_at_the_bound_and_answers_empty_out_of_range() {
        let t: NameTable<2, 4> = NameTable::new();
        t.set(0, "supervisor");
        assert_eq!(t.get(0), "supe");
        assert_eq!(t.get(1), "");
        assert_eq!(t.get(9), "");
        t.set(9, "ignored");
        assert_eq!(t.get(9), "");
    }

    #[test]
    fn a_read_racing_a_rewrite_never_mixes_two_names() {
        // One writer alternating two names of the same length, readers on other threads: every copy
        // must be one name whole, or empty (a reader that kept losing). Never half of each.
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        const L: usize = 64;
        let a = "a".repeat(L);
        let b = "b".repeat(L);
        let t: Arc<NameTable<1, L>> = Arc::new(NameTable::new());
        t.set(0, &a);
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3).map(|_| {
            let (t, stop, a, b) = (t.clone(), stop.clone(), a.clone(), b.clone());
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let n = t.get(0);
                    assert!(n == a.as_str() || n == b.as_str() || n == "", "torn read: {:?}", n);
                    reads += 1;
                }
                reads
            })
        }).collect();
        for i in 0..200_000 {
            t.set(0, if i % 2 == 0 { &b } else { &a });
        }
        stop.store(true, Ordering::Relaxed);
        let total: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(total > 0);
    }
}
