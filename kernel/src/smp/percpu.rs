// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Boot-time per-core arenas (§9, §26.6.1).
//!
//! A [`PerCore<T>`] is one array of N elements - N = the number of cores Limine actually reported -
//! allocated ONCE at boot from the frame allocator and never freed. It replaces the fixed
//! `[T; MAX_CORES]` statics so per-core memory is sized to the *machine* (a 4-core box reserves 4
//! slots, not `MAX_CORES`) instead of to a compile-time constant.
//!
//! This is a **bounded arena, not a heap** (§26.6.1): a single carve at boot, size = `N * sizeof(T)`,
//! no runtime alloc/free. `MAX_CORES` remains as a generous **sanity ceiling** - boot clamps N to it
//! (loudly) and it bounds the few things that must stay static - exactly as Linux keeps `NR_CPUS`
//! above its boot-sized per-CPU areas.
//!
//! All `unsafe` lives here (the carve + the pointer math), so call sites - including the grandfathered
//! `task/scheduler.rs` (§18.5) - stay `unsafe`-free behind the safe [`PerCore::get`], the same
//! discipline `SpinLock` uses.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Total cores the per-core arenas were sized for (Limine's count, clamped to `MAX_CORES`). Set once
/// at boot, before any AP starts; read everywhere a per-core loop needs the live width.
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

/// A boot-allocated array of one `T` per core.
///
/// `T: Sync` because a core's slot may be read by *other* cores (e.g. a shootdown address read by the
/// servicers), so the shared `&T` must be sound across cores; per-core *mutation* goes through `T`'s
/// own interior mutability (atomics), never `&mut`.
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

    /// Allocate `n` slots from the frame allocator and initialise slot `i` with `init(i)`.
    ///
    /// Call ONCE at boot, after the frame allocator is up and before any `get`. Panics (loud halt,
    /// §26.7) if the allocator cannot back the page-rounded request - per-core state is mandatory, so
    /// continuing without it is never safe.
    pub fn init_with(&self, n: usize, init: impl Fn(usize) -> T) {
        let bytes = n
            .checked_mul(core::mem::size_of::<T>())
            .expect("percpu: arena size overflow");
        let pages = (bytes + 0xFFF) / 0x1000;
        let phys = crate::memory::allocator::alloc_contiguous(pages.max(1))
            .expect("percpu: frame allocator could not back a per-core arena");
        let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
        let base = (hhdm + phys) as *mut T;
        // SAFETY: `base` covers `pages` freshly-allocated, page-aligned frames = at least
        // `n * sizeof(T)` bytes; each slot is written exactly once, here, before `base` is published,
        // so there is no concurrent access and no read of uninitialised memory. Alignment holds: a
        // 4 KiB page base is aligned for any `T` whose alignment is <= 4096 (every per-core type is
        // <= 64-byte aligned).
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
}
