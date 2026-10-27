// SPDX-License-Identifier: GPL-2.0-only
//! Delegated resource capabilities - §7.10, P2 (file-as-capability).
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
//! (the overflow table is tiny). Endpoint ids are allocated from 100 and reclaimed on
//! death for reuse (`ipc::free_endpoint_id`), so the live range stays bounded by the
//! routing table and does not reach `DELEGATED_BASE`; a loud guard in
//! `ipc::alloc_endpoint_id` makes a pathological collision a panic, never silent
//! cap-table corruption (invariant 12).
//!
//! **ABA-safe reuse (§7.4 decision 1).** A freed delegated id is reusable. If it
//! re-registered at generation 0, a *stale* cap from the id's previous life (also gen 0)
//! would spuriously re-validate - a capability-reuse hole. `allocate` therefore
//! re-registers a reused id at `prev_gen.bump()` (`register_resource_at_gen`), keeping the
//! generation strictly monotonic across lives, so a stale cap can never match a future life.

use super::cap::ResourceId;
use super::table::{get_resource_generation, mark_dead_resource, register_resource,
                   register_resource_at_gen, revoke_resource};
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
/// Pure state - no global side effects - so it is unit-testable in isolation.
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

    /// Free and return the first slot owned by `owner`, or `None` if it owns none. Used to
    /// reclaim a dying owner's whole band one slot at a time (so the global table is updated
    /// without holding the band lock across a second lock).
    fn take_one_owned(&mut self, owner: OwnerId) -> Option<usize> {
        for i in 0..DELEGATED_CAP {
            if self.owner[i] == Some(owner) {
                self.owner[i] = None;
                return Some(i);
            }
        }
        None
    }
}

static BAND: SpinLock<Band> = SpinLock::new(Band::new());

/// True if `id` falls in the delegated band (an O(1) range check - the discriminator
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

/// SEC-7: narrow an embedded cap for its RECEIVER before installing it. A DELEGATED-resource cap
/// (a file/socket, §7.10) is installed WITHOUT GRANT - the owning service (e.g. fs) mints it with
/// GRANT only so it can transfer it (§8.5 rule 1: an embedded cap must be grantable), but the
/// recipient only USES it (invokes); it must not re-delegate it - the owner controls delegation by
/// minting fresh caps. Endpoint caps are returned unchanged: re-delegating an endpoint (e.g. the
/// shell wiring a pipe stage, or a reply cap) is a legitimate part of the IPC model, so GRANT is
/// preserved for those. This is the default "narrow to need" §8.5 wants, enforced at the boundary.
///
/// Lives here rather than in `syscall/dispatch.rs`, its one caller, so the host tests can reach it
/// (backlog/90 E5: nothing at any level checked that SEC-7 removes GRANT).
pub fn narrow_embedded_for_receiver(cap: super::cap::Capability) -> super::cap::Capability {
    if is_delegated(cap.resource_id) {
        cap.without_grant()
    } else {
        cap
    }
}

/// Allocate a fresh delegated resource owned by `owner`, register it in the global table,
/// and return its `ResourceId`. `None` if the band is full (loud failure at the caller).
/// Generation is monotonic across reuse (ABA-safe, see module docs).
pub fn allocate(owner: OwnerId) -> Option<ResourceId> {
    let slot = BAND.lock().claim(owner)?;
    let id = id_of(slot);
    match get_resource_generation(id) {
        Some(g) => register_resource_at_gen(id, g.bump()), // reused id - strictly higher gen
        None => register_resource(id),                     // first life - gen 0
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

/// Reclaim **every** delegated resource owned by `owner` - called from the endpoint-death
/// path when a service dies (e.g. an `fs` restart, which is supported - Phase D). Without
/// this, a restartable owner's resources would orphan in the band (the new instance gets a
/// fresh endpoint id and never frees the old ones), leaking band capacity on every restart.
/// Each freed resource is marked **Dead** (gen bump) so any outstanding cap to it fails its
/// next use with `EndpointDead` (§7.5) - the same signal a client sees for any dead endpoint.
/// Returns the number reclaimed. Frees one slot per lock acquisition so the band lock is never
/// held across the global-table lock (`mark_dead_resource`).
pub fn release_owner(owner: OwnerId) -> u32 {
    let mut freed = 0u32;
    loop {
        let id = {
            let mut band = BAND.lock();
            band.take_one_owned(owner).map(id_of)
        };
        match id {
            Some(id) => { mark_dead_resource(id); freed += 1; }
            None => break,
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(n: u64) -> OwnerId {
        n
    }

    // The pure band logic is tested on a LOCAL Band (Boxed - 32 KiB), so no global
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
    fn take_one_owned_reclaims_only_that_owner() {
        let mut b = Box::new(Band::new());
        // Owner A claims two slots, owner B one - interleaved.
        let a1 = b.claim(ep(1)).unwrap();
        let _b1 = b.claim(ep(2)).unwrap();
        let a2 = b.claim(ep(1)).unwrap();
        // Drain everything owner A owns.
        let mut drained = 0;
        while let Some(slot) = b.take_one_owned(ep(1)) {
            assert!(slot == a1 || slot == a2);
            drained += 1;
        }
        assert_eq!(drained, 2);
        // Owner A now owns nothing; owner B is untouched.
        assert!(b.take_one_owned(ep(1)).is_none());
        assert!(b.take_one_owned(ep(2)).is_some());
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

    /// SEC-7: a delegated cap reaches its receiver without GRANT, and with every other right it had.
    /// An endpoint cap reaches it unchanged, GRANT included. The band's two edges are both stripped.
    #[test]
    fn sec7_strips_grant_from_a_delegated_cap_and_only_from_one() {
        use super::super::cap::Capability;
        use super::super::generation::Generation;
        use super::super::rights::Rights;
        let rw_grant = Rights::READ | Rights::WRITE | Rights::GRANT;
        let cap = |id| Capability { resource_id: ResourceId(id), rights: rw_grant, generation: Generation(3) };
        let last = DELEGATED_BASE + DELEGATED_CAP as u64 - 1;
        for id in [DELEGATED_BASE, DELEGATED_BASE + 17, last] {
            let n = narrow_embedded_for_receiver(cap(id));
            assert_eq!(n.rights.0, (Rights::READ | Rights::WRITE).0, "id {id}");
            assert_eq!((n.resource_id, n.generation.0), (ResourceId(id), 3));
        }
        for id in [100, DELEGATED_BASE - 1, last + 1] {
            assert!(narrow_embedded_for_receiver(cap(id)) == cap(id), "id {id} is an endpoint, kept whole");
        }
    }

    /// The cursor wraps: with the band filled to its last slot and one slot freed behind the cursor,
    /// the next claim finds that slot, and the one after that finds nothing.
    #[test]
    fn claim_wraps_to_a_slot_freed_behind_the_cursor() {
        let mut b = Box::new(Band::new());
        for _ in 0..DELEGATED_CAP { b.claim(ep(1)).unwrap(); }
        b.free(1);
        assert_eq!(b.claim(ep(2)), Some(1));
        assert_eq!(b.claim(ep(2)), None);
    }

    /// The cursor moves ON past what it handed out: a slot freed behind it is not the next one
    /// claimed, so a just-closed file's id is not reissued at once (it waits for the cursor to come
    /// round, and comes back at a higher generation then).
    #[test]
    fn a_slot_freed_behind_the_cursor_is_not_reissued_next() {
        let mut b = Box::new(Band::new());
        for want in 0..3 { assert_eq!(b.claim(ep(1)), Some(want)); }
        b.free(0);
        assert_eq!(b.claim(ep(1)), Some(3));
        // ...including the slot it handed out last.
        b.free(3);
        assert_eq!(b.claim(ep(1)), Some(4));
    }

    // The rest go through the REAL global band and resource table - the path `ResourceMint`,
    // `ResourceInvoke`, `ResourceRevoke` and endpoint death take. Tests run in parallel in one
    // process, so each uses owner ids no other test uses and checks only the ids it allocated.

    use super::super::cap::CapError;
    use super::super::rights::Rights;
    use super::super::table::{mint_cap, CapTable};

    #[test]
    fn allocate_registers_an_owned_resource_in_the_band() {
        let owner = ep(0xD1_0001);
        let id = allocate(owner).expect("the band has room");
        assert!(is_delegated(id));
        assert_eq!(owner_of(id), Some(owner));
        assert!(get_resource_generation(id).is_some(), "allocate registers it globally");
        // An id outside the band has no owner, whatever the band holds.
        assert_eq!(owner_of(ResourceId(100)), None);
        assert_eq!(owner_of(ResourceId(DELEGATED_BASE + DELEGATED_CAP as u64)), None);
        release(id);
    }

    #[test]
    fn release_frees_the_slot() {
        let owner = ep(0xD1_0002);
        let id = allocate(owner).unwrap();
        release(id);
        assert_eq!(owner_of(id), None);
        release(ResourceId(100)); // outside the band: nothing to free, and no panic
    }

    /// Ownership IS the check: a non-owner's revoke changes nothing; the owner's makes every
    /// outstanding cap `CapRevoked` and frees the slot; a second revoke is refused.
    #[test]
    fn revoke_owned_only_by_the_owner_and_it_invalidates_the_cap() {
        let (owner, other) = (ep(0xD1_0003), ep(0xD1_0004));
        let id = allocate(owner).unwrap();
        let mut t = Box::new(CapTable::empty());
        let slot = t.insert(mint_cap(id, Rights::READ)).unwrap();
        let before = get_resource_generation(id).unwrap();

        assert!(!revoke_owned(id, other));
        assert_eq!(owner_of(id), Some(owner));
        assert!(t.get(slot, Rights::READ).is_ok());

        assert!(revoke_owned(id, owner));
        assert_eq!(owner_of(id), None);
        assert!(get_resource_generation(id).unwrap().0 > before.0);
        assert_eq!(t.get(slot, Rights::READ), Err(CapError::CapRevoked));

        assert!(!revoke_owned(id, owner), "a freed id has no owner to revoke it");
        assert!(!revoke_owned(ResourceId(100), owner), "outside the band");
    }

    /// An owner's death reclaims everything it owns - each cap goes `EndpointDead` - and nothing
    /// anyone else owns. The count is exact.
    #[test]
    fn release_owner_reclaims_exactly_what_the_owner_held() {
        let (dying, survivor) = (ep(0xD1_0005), ep(0xD1_0006));
        let mut t = Box::new(CapTable::empty());
        let mut slots = [0usize; 3];
        for s in slots.iter_mut() {
            *s = t.insert(mint_cap(allocate(dying).unwrap(), Rights::READ)).unwrap();
        }
        let kept = allocate(survivor).unwrap();

        assert_eq!(release_owner(dying), 3);
        for s in slots {
            assert_eq!(t.get(s, Rights::READ), Err(CapError::EndpointDead));
        }
        assert_eq!(release_owner(dying), 0);
        assert_eq!(owner_of(kept), Some(survivor));
        release(kept);
    }
}
