// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Delegated resource capabilities — §7.10, P2 (file-as-capability).
//!
//! A *delegated resource* is a kernel-managed capability target whose **meaning is
//! defined by a service**, not the kernel (§4.4 amendment 2026-06-18). The kernel
//! mints, validates, routes, and revokes its caps exactly as for any resource; it
//! only ever tracks an opaque `ResourceId` and the **owning endpoint**. `fs` owns a
//! band of these and maps `ResourceId → file`; the kernel never learns what a file
//! is. See `docs/persistence.md` §7.4 for the full mechanism.
//!
//! **The band.** Delegated ids occupy `[DELEGATED_BASE, DELEGATED_BASE+DELEGATED_CAP)`,
//! kept inside `[0, DIRECT_CAP=8192)` so the global resource table direct-indexes them
//! (the overflow table is tiny). Endpoint ids climb monotonically from 100 and never
//! reach `DELEGATED_BASE` in practice (the restart-storm tests do ≤200 spawns); a loud
//! guard in `ipc::alloc_endpoint_id` makes a pathological collision a panic, never silent
//! cap-table corruption (invariant 12).
//!
//! **ABA-safe reuse (§7.4 decision 1).** A freed delegated id is reusable. If it
//! re-registered at generation 0, a *stale* cap from the id's previous life (also gen 0)
//! would spuriously re-validate — a capability-reuse hole. `allocate` therefore
//! re-registers a reused id at `prev_gen.bump()` (`register_resource_at_gen`), keeping the
//! generation strictly monotonic across lives, so a stale cap can never match a future life.

use super::cap::ResourceId;
use super::table::{get_resource_generation, register_resource, register_resource_at_gen,
                   revoke_resource};
use crate::smp::SpinLock;

// The owner of a delegated resource is identified by its endpoint id as a raw `u64`
// (`EndpointId.0`). Keeping the band free of the `ipc::endpoint` type lets the host test
// harness (`src/lib.rs`, which uses model types) compile and unit-test the pure band logic;
// the dispatch handlers convert at the boundary.
type OwnerId = u64;

/// First `ResourceId` of the delegated band. Above any realistic endpoint id, below
/// `DIRECT_CAP` (8192) so the global table direct-indexes the whole band.
pub const DELEGATED_BASE: u64 = 4096;
/// Number of simultaneously-live delegated resources (open file caps). Reuse makes the
/// *total* unbounded; this caps how many can be live at once (loud failure past it, §26.6).
pub const DELEGATED_CAP: usize = 2048; // band = [4096, 6144)

/// The owner-endpoint table for the band. `owner[i] == None` means slot `i` is free.
/// Pure state — no global side effects — so it is unit-testable in isolation.
struct Band {
    owner: [Option<OwnerId>; DELEGATED_CAP],
    next: usize, // bump cursor; wraps and scans for the next free slot
}

impl Band {
    const fn new() -> Self {
        Self { owner: [None; DELEGATED_CAP], next: 0 }
    }

    /// Claim the next free slot for `owner`; returns its index, or `None` if the band is full.
    fn claim(&mut self, owner: OwnerId) -> Option<usize> {
        for off in 0..DELEGATED_CAP {
            let i = (self.next + off) % DELEGATED_CAP;
            if self.owner[i].is_none() {
                self.owner[i] = Some(owner);
                self.next = (i + 1) % DELEGATED_CAP;
                return Some(i);
            }
        }
        None
    }

    fn owner_at(&self, slot: usize) -> Option<OwnerId> {
        self.owner.get(slot).copied().flatten()
    }

    fn is_owner(&self, slot: usize, who: OwnerId) -> bool {
        self.owner_at(slot) == Some(who)
    }

    fn free(&mut self, slot: usize) {
        if let Some(s) = self.owner.get_mut(slot) {
            *s = None;
        }
    }
}

static BAND: SpinLock<Band> = SpinLock::new(Band::new());

/// True if `id` falls in the delegated band (an O(1) range check — the discriminator
/// `handle_resource_invoke`/`_revoke` use; `handle_send` never consults it).
pub const fn is_delegated(id: ResourceId) -> bool {
    id.0 >= DELEGATED_BASE && id.0 < DELEGATED_BASE + DELEGATED_CAP as u64
}

fn slot_of(id: ResourceId) -> usize {
    (id.0 - DELEGATED_BASE) as usize
}

fn id_of(slot: usize) -> ResourceId {
    ResourceId(DELEGATED_BASE + slot as u64)
}

/// Allocate a fresh delegated resource owned by `owner`, register it in the global table,
/// and return its `ResourceId`. `None` if the band is full (loud failure at the caller).
/// Generation is monotonic across reuse (ABA-safe, see module docs).
pub fn allocate(owner: OwnerId) -> Option<ResourceId> {
    let slot = BAND.lock().claim(owner)?;
    let id = id_of(slot);
    match get_resource_generation(id) {
        Some(g) => register_resource_at_gen(id, g.bump()), // reused id — strictly higher gen
        None => register_resource(id),                     // first life — gen 0
    }
    Some(id)
}

/// The owning endpoint of a delegated resource, or `None` if `id` is not a live delegated
/// resource. Used to route a `ResourceInvoke` to the owner.
pub fn owner_of(id: ResourceId) -> Option<OwnerId> {
    if !is_delegated(id) {
        return None;
    }
    BAND.lock().owner_at(slot_of(id))
}

/// Release a just-allocated delegated id that could not be handed out (e.g. the caller's
/// cap table was full). Frees the owner slot; the global record persists so a later reuse
/// re-registers at a higher generation.
pub fn release(id: ResourceId) {
    if is_delegated(id) {
        BAND.lock().free(slot_of(id));
    }
}

/// Revoke a delegated resource `id`, but **only if `caller` owns it** (ownership is the
/// capability check, §3.1). Bumps the generation (Revoked) so every outstanding cap to it
/// goes stale (§7.5), then frees the slot for reuse. Returns `false` if `id` is not in the
/// band or `caller` is not its owner.
pub fn revoke_owned(id: ResourceId, caller: OwnerId) -> bool {
    if !is_delegated(id) {
        return false;
    }
    let slot = slot_of(id);
    let mut band = BAND.lock();
    if !band.is_owner(slot, caller) {
        return false;
    }
    revoke_resource(id); // gen bump + Revoked in the global table → stale caps
    band.free(slot);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u64) -> OwnerId {
        n
    }

    // The pure band logic is tested on a LOCAL Band (Boxed — 32 KiB), so no global
    // resource-table state is touched (mirrors the table.rs test convention).

    #[test]
    fn claim_returns_distinct_free_slots() {
        let mut b = Box::new(Band::new());
        let a = b.claim(ep(100)).unwrap();
        let c = b.claim(ep(100)).unwrap();
        assert_ne!(a, c);
        assert_eq!(b.owner_at(a), Some(ep(100)));
        assert_eq!(b.owner_at(c), Some(ep(100)));
    }

    #[test]
    fn free_slot_is_reclaimable() {
        let mut b = Box::new(Band::new());
        let s = b.claim(ep(7)).unwrap();
        b.free(s);
        assert_eq!(b.owner_at(s), None);
        // The freed slot is handed out again (bump cursor wraps to find it).
        let mut reclaimed = None;
        for _ in 0..DELEGATED_CAP {
            let n = b.claim(ep(9)).unwrap();
            if n == s { reclaimed = Some(n); break; }
        }
        assert_eq!(reclaimed, Some(s));
    }

    #[test]
    fn is_owner_matches_only_the_claimer() {
        let mut b = Box::new(Band::new());
        let s = b.claim(ep(42)).unwrap();
        assert!(b.is_owner(s, ep(42)));
        assert!(!b.is_owner(s, ep(43)));
    }

    #[test]
    fn full_band_returns_none() {
        let mut b = Box::new(Band::new());
        for _ in 0..DELEGATED_CAP {
            assert!(b.claim(ep(1)).is_some());
        }
        assert!(b.claim(ep(1)).is_none()); // band exhausted → loud None
    }

    #[test]
    fn is_delegated_band_bounds() {
        assert!(!is_delegated(ResourceId(DELEGATED_BASE - 1)));
        assert!(is_delegated(ResourceId(DELEGATED_BASE)));
        assert!(is_delegated(ResourceId(DELEGATED_BASE + DELEGATED_CAP as u64 - 1)));
        assert!(!is_delegated(ResourceId(DELEGATED_BASE + DELEGATED_CAP as u64)));
        // Endpoint ids (climb from 100) and stable resources (1..7) are never delegated.
        assert!(!is_delegated(ResourceId(7)));
        assert!(!is_delegated(ResourceId(100)));
        assert!(!is_delegated(ResourceId(500)));
    }

    #[test]
    fn slot_id_roundtrip() {
        for slot in [0usize, 1, 1000, DELEGATED_CAP - 1] {
            assert_eq!(slot_of(id_of(slot)), slot);
        }
    }
}
