// SPDX-License-Identifier: GPL-2.0-only
//! Boot-time per-core arenas (§9, §26.6.1).
//!
//! [`PerCore<T>`] / [`PerCoreMut<T>`] are arrays of one element per core - N = the cores Limine
//! actually reported - allocated ONCE at boot from the frame allocator and never freed. They replace
//! the fixed `[T; MAX_CORES]` statics so per-core memory is sized to the *machine* (a 4-core box
//! reserves 4 slots, a 512-core box reserves 512) instead of to a compile-time constant.
//!
//! This is a **bounded arena, not a heap** (§26.6.1): a single carve at boot, size = `N * sizeof(T)`,
//! no runtime alloc/free. There is **no fixed core ceiling** any more - N is sized directly to the
//! machine's real core count, and the only bound is RAM (a carve panics loudly, §26.7, if it cannot be
//! backed). Nothing is a `[_; MAX_CORES]` array.
//!
//! Two flavours by access pattern:
//!   - [`PerCore<T>`] hands out a shared `&T` (`get`); `T: Sync`, mutation via atomics. Read-mostly
//!     per-core state that other cores also read (e.g. shootdown slots).
//!   - [`PerCoreMut<T>`] hands out a raw `*mut T` (`as_mut_ptr`); for state the OWNING core writes
//!     (saved register contexts, deferred-free lists). Sound under the per-core single-owner invariant.
//!
//! All `unsafe` lives here (the carve + pointer math), so call sites - including the grandfathered
//! `task/scheduler.rs` (§18.5) - stay `unsafe`-free behind the safe accessors, the `SpinLock` discipline.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Total cores the per-core arenas were sized for (Limine's live count, no ceiling). Set once at boot,
/// before any AP starts; read everywhere a per-core loop needs the live width.
static NUM_CORES: AtomicUsize = AtomicUsize::new(1);

/// Record the per-core arena width. Call ONCE at boot (after the core count is known, before APs run).
pub fn set_num_cores(n: usize) {
    NUM_CORES.store(n, Ordering::Release);
}

/// The width every per-core arena was allocated for (the live system core count).
#[inline]
pub fn num_cores() -> usize {
    NUM_CORES.load(Ordering::Acquire)
}

/// Carve `n` UNINITIALISED slots of `T` from the frame allocator and return the base pointer (in the
/// HHDM). Shared by `PerCore` and `PerCoreMut`. Page-rounded; panics (loud halt, §26.7) if the
/// allocator cannot back it - per-core state is mandatory. Safe: returns a raw pointer, forms no
/// reference and reads nothing.
fn alloc_arena<T>(n: usize) -> *mut T {
    let bytes = n
        .checked_mul(core::mem::size_of::<T>())
        .expect("percpu: arena size overflow");
    let pages = (bytes + 0xFFF) / 0x1000;
    let phys = crate::memory::allocator::alloc_contiguous(pages.max(1))
        .expect("percpu: frame allocator could not back a per-core arena");
    let hhdm = crate::arch::imp::page_tables::get_hhdm_offset();
    (hhdm + phys) as *mut T
}

/// A boot-allocated array of one `T` per core, accessed by SHARED reference ([`get`](PerCore::get)).
///
/// `T: Sync` because a core's slot may be read by *other* cores (e.g. a shootdown address read by the
/// servicers); per-core *mutation* goes through `T`'s own interior mutability (atomics), never `&mut`.
pub struct PerCore<T: Sync + 'static> {
    base: AtomicPtr<T>,
}

// SAFETY: the only interior pointer is published exactly once at boot (Release in `init_with`) and
// only read (Acquire in `get`) thereafter; `T: Sync` makes the pointed-to slots safe to share.
unsafe impl<T: Sync + 'static> Sync for PerCore<T> {}

impl<T: Sync + 'static> PerCore<T> {
    /// A not-yet-allocated arena. `init_with` must run before any `get`.
    pub const fn new() -> Self {
        Self { base: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Allocate `n` slots and initialise slot `i` with `init(i)`. Call ONCE at boot, after the frame
    /// allocator is up and before any `get`.
    pub fn init_with(&self, n: usize, init: impl Fn(usize) -> T) {
        let base = alloc_arena::<T>(n);
        // SAFETY: `base` covers `n` freshly-allocated, page-aligned slots; each is written exactly
        // once, here, before `base` is published, so there is no concurrent access and no read of
        // uninitialised memory. Alignment holds: a 4 KiB page base suits any `T` aligned <= 4096.
        for i in 0..n {
            unsafe { ptr::write(base.add(i), init(i)); }
        }
        self.base.store(base, Ordering::Release);
    }

    /// Borrow core `core`'s slot. `core` must be `< num_cores()` (callers index by a valid core id).
    #[inline]
    pub fn get(&self, core: usize) -> &T {
        let base = self.base.load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "percpu: arena used before init_with");
        debug_assert!(core < num_cores(), "percpu: core index {core} out of range");
        // SAFETY: `base` points to `num_cores()` initialised, never-freed slots; `core < num_cores()`
        // by the caller's contract; `T: Sync` makes the shared `&T` sound across cores.
        unsafe { &*base.add(core) }
    }

    /// True once `init_with` has run (the arena is allocated). Lets a caller that might run BEFORE boot
    /// finishes - e.g. `pf_handler` on an early kernel fault, before `percpu_init` - avoid touching an
    /// unallocated arena.
    #[inline]
    pub fn initialised(&self) -> bool {
        !self.base.load(Ordering::Acquire).is_null()
    }
}

/// A boot-allocated array of one `T` per core, accessed by OWNER-MUTABLE raw pointer
/// ([`as_mut_ptr`](PerCoreMut::as_mut_ptr)).
///
/// For per-core state the OWNING core writes - saved register contexts, deferred-free lists - which
/// can't use `PerCore`'s shared `&T`. Sound under the **per-core single-owner invariant**: each core
/// only ever dereferences its OWN slot, so no two cores alias one. `T` need not be `Sync` - the
/// invariant, not the type, provides the guarantee.
pub struct PerCoreMut<T: 'static> {
    base: AtomicPtr<T>,
}

// SAFETY: shared across cores only as a container; the single-owner invariant - each core dereferences
// only `as_mut_ptr(its own core id)` - means no slot is ever aliased, so this is sound despite the
// owner-mutable access.
unsafe impl<T: 'static> Sync for PerCoreMut<T> {}

impl<T: 'static> PerCoreMut<T> {
    /// A not-yet-allocated arena. `init_with` must run before any `as_mut_ptr`.
    pub const fn new() -> Self {
        Self { base: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Allocate `n` slots and initialise slot `i` with `init(i)`. Call ONCE at boot, after the frame
    /// allocator is up and before any `as_mut_ptr`.
    pub fn init_with(&self, n: usize, init: impl Fn(usize) -> T) {
        let base = alloc_arena::<T>(n);
        // SAFETY: as `PerCore::init_with` - `n` fresh page-aligned slots, each written exactly once
        // here before `base` is published, so no concurrent access and no read of uninitialised memory.
        for i in 0..n {
            unsafe { ptr::write(base.add(i), init(i)); }
        }
        self.base.store(base, Ordering::Release);
    }

    /// Raw MUTABLE pointer to core `core`'s slot. Safe to CALL (forms no reference, reads nothing); the
    /// DEREF is the caller's `unsafe`, sound only under the per-core single-owner invariant (only the
    /// owning core dereferences its slot, no aliasing `&mut`).
    #[inline]
    pub fn as_mut_ptr(&self, core: usize) -> *mut T {
        let base = self.base.load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "percpu: arena used before init_with");
        debug_assert!(core < num_cores(), "percpu: core index {core} out of range");
        // SAFETY: `base` points to `num_cores()` slots; `core < num_cores()` by the caller's contract.
        unsafe { base.add(core) }
    }

    /// Raw const pointer to core `core`'s slot (for read-only / `next` arguments).
    #[inline]
    pub fn as_ptr(&self, core: usize) -> *const T {
        self.as_mut_ptr(core) as *const T
    }

    /// True once `init_with` has run. Lets a caller that might run BEFORE the arena is allocated fall
    /// back to a bootstrap slot (e.g. the BSP's syscall GS data, set in `init_bsp` before `percpu_init`).
    #[inline]
    pub fn initialised(&self) -> bool {
        !self.base.load(Ordering::Acquire).is_null()
    }
}

/// Carve a flat `[AtomicU64; n]` from the frame allocator (in the HHDM), every element initialised to
/// `init`, and return it as a `'static` slice. For per-core state whose *width scales with the core
/// count* - the TLB-shootdown ack bitmask is `num_cores * ceil(num_cores/64)` words, which no fixed
/// `PerCore<[_; K]>` can size dynamically. A bounded arena (§26.6.1): one boot carve, never freed.
/// Panics (loud halt, §26.7) if the allocator cannot back it.
pub fn alloc_atomic_u64_slice(n: usize, init: u64) -> &'static [portable_atomic::AtomicU64] {
    use portable_atomic::AtomicU64;
    let base = alloc_arena::<AtomicU64>(n.max(1));
    // SAFETY: `base` covers `n` freshly-allocated, page-aligned slots (alloc_arena rounds up); each is
    // written exactly once here before any reader can reach the slice, so no concurrent access and no
    // read of uninitialised memory.
    for i in 0..n {
        unsafe { ptr::write(base.add(i), AtomicU64::new(init)); }
    }
    // SAFETY: `base..base+n` is initialised, never freed, and `AtomicU64: Sync`, so a shared `'static`
    // slice is sound across cores.
    unsafe { core::slice::from_raw_parts(base, n) }
}
