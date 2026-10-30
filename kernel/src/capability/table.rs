// SPDX-License-Identifier: GPL-2.0-only
//! Per-task capability table and the global resource generation registry - §7.8.
//!
//! Two structures live here:
//!
//! 1. `CapTable` - one per task; maps a slot index to a `Capability`.
//!    Populated at spawn time from the spawn request (§13.6 - the kernel cannot
//!    read a contract); modified by cap transfer, `DeriveCap`, `RemoveCap`, the
//!    caps a syscall mints into the caller (`AcquireSendCap`, `ResourceMint`, the
//!    spawn syscalls' returned endpoint cap), and task death.
//!
//! 2. `GlobalResourceTable` - one per kernel; maps `ResourceId` to its
//!    current generation and liveness. Consulted on every cap validation.
//!
//! Concurrency (§7.8): v1 uses a single global `SpinLock` - the single global lock §7.8 approves,
//! plain mutual exclusion. EVERYTHING serialises: a read (lookup + generation check)
//! takes the same exclusive lock a write (insertion on spawn, removal on death) does, and can spin
//! behind it. This said "reads are concurrent", which was a claim about an `RwLock` the kernel does
//! not have. A v2 sharded or RCU design requires benchmarks before adoption.

use super::cap::{CapError, Capability, ResourceId};
use super::generation::Generation;
use super::rights::Rights;
use crate::smp::SpinLock;

const MAX_CAPS_PER_TASK: usize = 64;

/// One task's capability table. Not shared between tasks.
pub struct CapTable {
    slots: [Option<Capability>; MAX_CAPS_PER_TASK],
}

impl CapTable {
    pub const fn empty() -> Self {
        Self { slots: [None; MAX_CAPS_PER_TASK] }
    }

    /// Look up and validate a capability by slot index.
    ///
    /// Returns `Err(CapNotHeld)` if the slot is empty, `Err(EndpointDead)` or
    /// `Err(CapRevoked)` on a generation mismatch (distinguished by liveness),
    /// and `Err(CapInsufficientRights)` if the right is absent.
    pub fn get(&self, slot: usize, required_right: Rights) -> Result<Capability, CapError> {
        let cap = self.slots.get(slot)
            .and_then(|s| s.as_ref())
            .ok_or(CapError::CapNotHeld)?;

        // Bound first, so the guard drops at the end of THIS statement. As the match's scrutinee it lived
        // for the whole match, and the not-found line below was printed holding the global capability
        // lock that every syscall's lookup takes (backlog/90 D28).
        let found = GLOBAL_RESOURCES.lock_irq().get_record(cap.resource_id);
        let record = match found {
            Some(r) => r,
            None => {
                if cap.resource_id.0 >= 100 {
                    crate::kprintln!("cap::get: ResourceId({}) not found in GLOBAL_RESOURCES",
                        cap.resource_id.0);
                }
                return Err(CapError::CapNotHeld);
            }
        };

        if !cap.generation.matches(record.generation) {
            // Only log for endpoint resources (id>=100) to avoid startup test noise.
            if cap.resource_id.0 >= 100 {
                crate::kprintln!("cap::get: ResourceId({}) gen mismatch cap={} rec={} liveness={:?}",
                    cap.resource_id.0, cap.generation.0, record.generation.0, record.liveness);
            }
            return Err(match record.liveness {
                Liveness::Dead    => CapError::EndpointDead,
                Liveness::Revoked => CapError::CapRevoked,
                // A LIVE record at another generation: the id was reused after the cap's resource
                // ended. For an ENDPOINT that is a death (ids are reused LIFO, so a respawn or any new
                // spawn usually takes the id straight back), and saying CapRevoked told a sender its
                // capability was taken away when the service it named had died - 7.7 keeps the two
                // apart. Found when the SDK stopped folding -6 into EndpointDead: adversarial A5 sent
                // after a kill and, when another probe's spawn took the id first, got CapRevoked
                // (2026-10-27). A delegated id (a file) that is live again was revoked or released and
                // reissued, so it stays CapRevoked: the capability is finished, not its owner.
                Liveness::Alive => if super::delegated::is_delegated(cap.resource_id) {
                    CapError::CapRevoked
                } else {
                    CapError::EndpointDead
                },
            });
        }

        // A record that has ended refuses EVERY capability, including one carrying its current
        // generation. Such a cap can exist: `mint_cap` reads whatever the record holds, so a mint that
        // races a death (AcquireSendCap after its name lookup, a spawn handing back an endpoint whose
        // service already died) is issued at the dead generation. It used to validate here, and only the
        // routing table refused the send (backlog/90 D25).
        match record.liveness {
            Liveness::Alive   => {}
            Liveness::Dead    => return Err(CapError::EndpointDead),
            Liveness::Revoked => return Err(CapError::CapRevoked),
        }

        if !cap.rights.contains(required_right) {
            return Err(CapError::CapInsufficientRights);
        }

        Ok(*cap)
    }

    /// Insert a capability into the first free slot. Returns the slot index.
    pub fn insert(&mut self, cap: Capability) -> Result<usize, CapError> {
        self.slots.iter_mut()
            .enumerate()
            .find(|(_, s)| s.is_none())
            .map(|(i, s)| { *s = Some(cap); i })
            .ok_or(CapError::CapNotHeld) // table full
    }

    /// Remove the capability at `slot`.
    pub fn remove(&mut self, slot: usize) -> Option<Capability> {
        self.slots.get_mut(slot)?.take()
    }

    /// Iterate over all held capabilities in this table.
    ///
    /// Used by `invariants::assertions::assert_cap_table_consistent` (§7.8).
    pub fn for_each_slot<F: FnMut(&Capability)>(&self, mut f: F) {
        for slot in &self.slots {
            if let Some(cap) = slot {
                f(cap);
            }
        }
    }

    /// Return true if this table holds a capability on `rid` carrying
    /// `required_right`. Searches by resource rather than slot - used to gate
    /// syscalls that consume all argument registers and so cannot pass a cap-slot
    /// (the introspection syscalls, §3.1). See `docs/introspection-capability.md`.
    ///
    /// **Intended for STABLE kernel resources only** (generation 0 forever -
    /// `LOG_WRITE_RESOURCE`, `SPAWN`, `INTROSPECT`, …). Those are never revoked, so a held
    /// cap always matches the record; we deliberately skip the generation check -
    /// and the `GLOBAL_RESOURCES` lock it would need - to keep this off the syscall
    /// hot path (the v1 global lock is the §7.8 contention point). Do NOT use for
    /// revocable/endpoint resources, where a stale cap must fail its gen check.
    pub fn holds_resource(&self, rid: ResourceId, required_right: Rights) -> bool {
        for slot in &self.slots {
            if let Some(cap) = slot {
                if cap.resource_id == rid && cap.rights.contains(required_right) {
                    return true;
                }
            }
        }
        false
    }
}

// ---

#[derive(Debug, Clone, Copy)]
pub struct ResourceRecord {
    pub generation: Generation,
    pub liveness: Liveness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    /// Explicitly revoked by the supervisor; returns `CapRevoked` on next use.
    Revoked,
    /// Endpoint/service died; returns `EndpointDead` on next use.
    Dead,
}

// IDs below this threshold are stored in a direct-indexed array for O(1) lookup.
// All endpoint ResourceIds (monotonically allocated from ~100) and well-known
// kernel resource IDs (LOG_WRITE, SPAWN, etc., all < 100) fall within this range.
// P2 adds ~1000 entries, P8 adds ~2500 - both fit comfortably within 8192.
const DIRECT_CAP: usize = 8192;
// Large IDs (e.g. cap-subsystem self-test resources like 57005, 57007) are rare
// and handled by a small linear-scan overflow table.
const OVERFLOW_CAP: usize = 32;

const EMPTY_RECORD: ResourceRecord =
    ResourceRecord { generation: Generation::INITIAL, liveness: Liveness::Alive };
const EMPTY_OVERFLOW: (ResourceId, ResourceRecord) =
    (ResourceId(0), EMPTY_RECORD);

struct GlobalResourceTable {
    // Direct-indexed by ResourceId.0 for IDs in [0, DIRECT_CAP).
    records:      [ResourceRecord; DIRECT_CAP],
    present:      [bool; DIRECT_CAP],
    // Linear scan for ResourceId.0 >= DIRECT_CAP (cap-test special resources only).
    overflow:     [(ResourceId, ResourceRecord); OVERFLOW_CAP],
    overflow_len: usize,
}

impl GlobalResourceTable {
    const fn new() -> Self {
        Self {
            records:      [EMPTY_RECORD; DIRECT_CAP],
            present:      [false; DIRECT_CAP],
            overflow:     [EMPTY_OVERFLOW; OVERFLOW_CAP],
            overflow_len: 0,
        }
    }

    fn get_record(&self, id: ResourceId) -> Option<ResourceRecord> {
        let i = id.0 as usize;
        if i < DIRECT_CAP {
            if self.present[i] { Some(self.records[i]) } else { None }
        } else {
            self.overflow[..self.overflow_len]
                .iter()
                .find(|(rid, _)| *rid == id)
                .map(|(_, r)| *r)
        }
    }

    fn bump_generation(&mut self, id: ResourceId, liveness: Liveness) {
        // SEC-11: no STABLE gate resource (ids 1-19, `capability/mod.rs`: LOG_WRITE .. POWER_OFF) is
        // ever revoked or killed. `holds_resource` (the by-holdings gate - Kill, Reboot, PowerOff, ResourceMint,
        // the introspection reads, the device syscalls, SpawnImage's IMAGE_SPAWN) validates those WITHOUT a
        // generation check, which is sound only while they stay un-revocable. Revocable ids are always >= 100 (endpoints) or in the delegated band. This
        // pins the invariant, so a future change that makes a gated resource revocable fails loudly
        // rather than silently letting a revoked holder keep passing the gate. An `assert!`, in RELEASE
        // too (backlog/90 E6): it was a `debug_assert!`, so the images that run on hardware carried no
        // guard at all, and a bump here would be corrupted capability state, which 6.2 says halts every
        // core. One integer compare, on a path that runs only at a resource's death or revocation.
        assert!(id.0 >= 100,
            "SEC-11: bump_generation on stable gate resource {} would break holds_resource gen-safety", id.0);
        let i = id.0 as usize;
        if i < DIRECT_CAP {
            if self.present[i] {
                self.records[i].generation = ended_generation(self.records[i].generation);
                self.records[i].liveness   = liveness;
            }
        } else if let Some((_, r)) = self.overflow[..self.overflow_len]
                .iter_mut().find(|(rid, _)| *rid == id) {
            r.generation = ended_generation(r.generation);
            r.liveness   = liveness;
        }
    }

    fn register(&mut self, id: ResourceId) {
        self.register_at_gen(id, Generation::INITIAL);
    }

    fn register_at_gen(&mut self, id: ResourceId, gen: Generation) {
        let i = id.0 as usize;
        if i < DIRECT_CAP {
            self.records[i] = ResourceRecord { generation: gen, liveness: Liveness::Alive };
            self.present[i] = true;
        } else {
            assert!(self.overflow_len < OVERFLOW_CAP,
                "GlobalResourceTable overflow full - increase OVERFLOW_CAP");
            self.overflow[self.overflow_len] =
                (id, ResourceRecord { generation: gen, liveness: Liveness::Alive });
            self.overflow_len += 1;
        }
    }
}

/// The generation a record takes when it ends: above its own, and a value the global counter will
/// never issue again. An endpoint id is reused at `next_generation()`, and `bump()` alone gave the
/// ended record `gen + 1`, which the counter could hand straight back to the id's next life: a cap
/// minted on the dead record would then have validated against the new service, and no-resurrection
/// rested on which spawn took the counter's next value (backlog/90 D25). Taking a counter value makes
/// the dead generation unique. A delegated id is reused at the record's own `bump()`, above it either way.
fn ended_generation(g: Generation) -> Generation {
    let bumped = g.bump();
    let issued = super::generation::next_generation();
    if issued.0 > bumped.0 { issued } else { bumped }
}

static GLOBAL_RESOURCES: SpinLock<GlobalResourceTable> =
    SpinLock::new(GlobalResourceTable::new());

pub fn init_global() {
    // SpinLock<GlobalResourceTable> is self-initializing; nothing to do.
}

/// Register a new resource in the global table with generation 0 and liveness Alive.
pub fn register_resource(id: ResourceId) {
    GLOBAL_RESOURCES.lock_irq().register(id);
}

/// Register a new resource starting at a specific generation (used on respawn - §7.5 P2/P8).
pub fn register_resource_at_gen(id: ResourceId, gen: Generation) {
    GLOBAL_RESOURCES.lock_irq().register_at_gen(id, gen);
}

/// Return the current generation of a registered resource, or None if not found.
pub fn get_resource_generation(id: ResourceId) -> Option<Generation> {
    GLOBAL_RESOURCES.lock_irq().get_record(id).map(|r| r.generation)
}

/// Return the rights of the cap in `slot`, without validating the generation.
///
/// Used by the `QueryCapRights` syscall (§9 Phase 2 P3).
pub fn cap_read_rights(slots: &CapTable, slot: usize) -> Option<super::rights::Rights> {
    slots.slots.get(slot)?.as_ref().map(|c| c.rights)
}

/// Mint a capability for `id` with the given rights at its current generation.
/// Panics if the resource is not registered.
pub fn mint_cap(id: ResourceId, rights: Rights) -> Capability {
    let record = GLOBAL_RESOURCES.lock_irq().get_record(id)
        .expect("mint_cap: resource not registered");
    Capability { resource_id: id, rights, generation: record.generation }
}

/// Mint a capability for `id` at a generation the caller READ, not the one the record holds now.
///
/// For a mint whose target was resolved earlier (`names::lookup_then`): if the id has since been freed and
/// reused, this cap carries the old life's generation and fails its check on first use, where `mint_cap`
/// would have handed out a live cap to whoever holds the id now (backlog/90 D5).
pub fn mint_cap_at(id: ResourceId, rights: Rights, generation: Generation) -> Capability {
    Capability { resource_id: id, rights, generation }
}

/// Bump generation and mark as Dead (endpoint/service terminated → `EndpointDead`).
pub fn mark_dead_resource(id: ResourceId) {
    GLOBAL_RESOURCES.lock_irq().bump_generation(id, Liveness::Dead);
}

/// Bump generation and mark as Revoked (explicit supervisor revocation → `CapRevoked`).
pub fn revoke_resource(id: ResourceId) {
    GLOBAL_RESOURCES.lock_irq().bump_generation(id, Liveness::Revoked);
}

/// Legacy alias; use `mark_dead_resource` or `revoke_resource` for new code.
pub fn bump_resource_generation(id: ResourceId) {
    mark_dead_resource(id)
}

#[cfg(test)]
mod tests {
    use super::{CapTable, GlobalResourceTable, Liveness, MAX_CAPS_PER_TASK, DIRECT_CAP};
    use crate::capability::cap::{Capability, ResourceId};
    use crate::capability::generation::Generation;
    use crate::capability::rights::Rights;
    use proptest::prelude::*;

    fn make_cap(id: u64, rights_bits: u8, gen: u32) -> Capability {
        Capability {
            resource_id: ResourceId(id),
            rights: Rights(rights_bits & 0b0011_1111),
            generation: Generation(gen),
        }
    }

    // --- GlobalResourceTable properties (§7.5, §22 P2, P8) -----------------
    // Each test creates a fresh LOCAL instance; no global state is touched.
    // Box avoids placing the ~73 KiB struct on the test stack.

    proptest! {
        /// After register(id), get_record(id) is Some with generation 0 and Alive.
        #[test]
        fn registered_resource_is_findable_at_gen_zero(
            id in 0u64..(DIRECT_CAP as u64),
        ) {
            let mut t = Box::new(GlobalResourceTable::new());
            t.register(ResourceId(id));
            let rec = t.get_record(ResourceId(id));
            prop_assert!(rec.is_some());
            prop_assert_eq!(rec.unwrap().generation.0, 0);
            prop_assert_eq!(rec.unwrap().liveness, Liveness::Alive);
        }

        /// Before register, get_record returns None.
        #[test]
        fn unregistered_resource_not_found(
            id in 0u64..(DIRECT_CAP as u64),
        ) {
            let t = Box::new(GlobalResourceTable::new());
            prop_assert!(t.get_record(ResourceId(id)).is_none());
        }

        // Generators for the BUMP properties start at 100, not 0. Ids below 100 are the STABLE gate
        // resources (SEC-11, `bump_generation` above): they are never revoked, and `holds_resource`
        // validates them without a generation check, which is sound only while that holds. Sweeping
        // from 0 fabricated a reserved id and then asserted that the forbidden bump succeeded - so
        // these three failed permanently, on a correct kernel, for a reason that was about the test.
        // Three always-red tests train everyone to read `94 passed - 3 failed` as green, which is how
        // a real regression walks in unnoticed (§22.4: a test failing for the wrong reason is a
        // failure of the test, not the kernel).
        /// bump_generation is strictly monotonic per resource (§7.5 P2).
        #[test]
        fn bump_generation_is_strictly_monotonic(
            id    in 100u64..(DIRECT_CAP as u64),
            bumps in 1usize..=8,
        ) {
            let mut t = Box::new(GlobalResourceTable::new());
            t.register(ResourceId(id));
            let mut prev = t.get_record(ResourceId(id)).unwrap().generation;
            for _ in 0..bumps {
                t.bump_generation(ResourceId(id), Liveness::Dead);
                let next = t.get_record(ResourceId(id)).unwrap().generation;
                prop_assert!(next.0 > prev.0, "gen did not increase: {} -> {}", prev.0, next.0);
                prev = next;
            }
        }

        /// After bump_generation with Dead liveness, get_record returns Dead.
        #[test]
        fn bump_to_dead_sets_liveness_dead(id in 100u64..(DIRECT_CAP as u64)) {
            let mut t = Box::new(GlobalResourceTable::new());
            t.register(ResourceId(id));
            t.bump_generation(ResourceId(id), Liveness::Dead);
            prop_assert_eq!(t.get_record(ResourceId(id)).unwrap().liveness, Liveness::Dead);
        }

        /// After bump_generation with Revoked liveness, get_record returns Revoked.
        #[test]
        fn bump_to_revoked_sets_liveness_revoked(id in 100u64..(DIRECT_CAP as u64)) {
            let mut t = Box::new(GlobalResourceTable::new());
            t.register(ResourceId(id));
            t.bump_generation(ResourceId(id), Liveness::Revoked);
            prop_assert_eq!(t.get_record(ResourceId(id)).unwrap().liveness, Liveness::Revoked);
        }
    }

    // --- CapTable properties ------------------------------------------------
    // insert/remove do not touch GLOBAL_RESOURCES - entirely local state.

    proptest! {
        /// Inserting n caps (n ≤ MAX_CAPS_PER_TASK) always succeeds.
        #[test]
        fn cap_table_insert_within_capacity(n in 0usize..=MAX_CAPS_PER_TASK) {
            let mut ct = CapTable::empty();
            for i in 0..n {
                prop_assert!(ct.insert(make_cap(i as u64, 0b0000_0100, 0)).is_ok());
            }
        }

        /// After inserting MAX_CAPS_PER_TASK caps the table is full; next insert fails.
        #[test]
        fn cap_table_full_rejects_next_insert(_seed in any::<u8>()) {
            let mut ct = CapTable::empty();
            for i in 0..MAX_CAPS_PER_TASK {
                ct.insert(make_cap(i as u64, 0b0000_0100, 0)).unwrap();
            }
            prop_assert!(ct.insert(make_cap(9999, 0b0000_0100, 0)).is_err());
        }

        /// Every inserted cap can be removed exactly once; second remove returns None.
        #[test]
        fn remove_is_idempotent_after_first_call(n in 1usize..=MAX_CAPS_PER_TASK) {
            let mut ct = CapTable::empty();
            let mut slots = Vec::new();
            for i in 0..n {
                let slot = ct.insert(make_cap(i as u64, 0b0000_0100, 0)).unwrap();
                slots.push(slot);
            }
            for &slot in &slots {
                prop_assert!(ct.remove(slot).is_some(), "first remove returned None");
                prop_assert!(ct.remove(slot).is_none(), "second remove should be None");
            }
        }

        /// Slots returned by insert are always < MAX_CAPS_PER_TASK.
        #[test]
        fn inserted_slot_is_within_bounds(n in 1usize..=MAX_CAPS_PER_TASK) {
            let mut ct = CapTable::empty();
            for i in 0..n {
                let slot = ct.insert(make_cap(i as u64, 0b0000_0100, 0)).unwrap();
                prop_assert!(slot < MAX_CAPS_PER_TASK);
            }
        }
    }

    // --- The GLOBAL path: what a syscall really calls (backlog/90 E5) --------------------------
    //
    // Everything above builds a LOCAL GlobalResourceTable, so `CapTable::get`, `holds_resource`,
    // `mint_cap` and the revoke and death paths - the ones every syscall goes through - were run by no
    // host test, and the 2026-10-27 mutation run showed it: deleting get's rights check, making
    // holds_resource return true, and turning revoke_resource into a no-op all survived. These use the
    // real GLOBAL_RESOURCES. Tests run on parallel threads in one process, so each owns its ids.

    use super::{cap_read_rights, get_resource_generation, mark_dead_resource, mint_cap,
                register_resource, register_resource_at_gen, revoke_resource, bump_resource_generation};
    use crate::capability::cap::CapError;

    #[test]
    fn get_refuses_a_right_the_cap_does_not_hold() {
        register_resource(ResourceId(5001));
        let mut ct = CapTable::empty();
        let slot = ct.insert(mint_cap(ResourceId(5001), Rights::READ)).unwrap();
        assert_eq!(ct.get(slot, Rights::READ).map(|c| c.resource_id), Ok(ResourceId(5001)));
        assert_eq!(ct.get(slot, Rights::WRITE).unwrap_err(), CapError::CapInsufficientRights);
        assert_eq!(ct.get(slot + 1, Rights::READ).unwrap_err(), CapError::CapNotHeld);
    }

    #[test]
    fn get_after_a_revoke_is_cap_revoked() {
        register_resource(ResourceId(5002));
        let mut ct = CapTable::empty();
        let slot = ct.insert(mint_cap(ResourceId(5002), Rights::SEND)).unwrap();
        revoke_resource(ResourceId(5002));
        assert_eq!(ct.get(slot, Rights::SEND).unwrap_err(), CapError::CapRevoked);
    }

    #[test]
    fn get_after_a_death_is_endpoint_dead() {
        for id in [5003u64, 5004] {
            register_resource(ResourceId(id));
            let mut ct = CapTable::empty();
            let slot = ct.insert(mint_cap(ResourceId(id), Rights::SEND)).unwrap();
            if id == 5003 { mark_dead_resource(ResourceId(id)) } else { bump_resource_generation(ResourceId(id)) }
            assert_eq!(ct.get(slot, Rights::SEND).unwrap_err(), CapError::EndpointDead);
            // A cap minted AFTER the death carries the dead record's generation, and is refused too: an
            // ended record validates nothing (backlog/90 D25). This asserted the opposite until then.
            let late = ct.insert(mint_cap(ResourceId(id), Rights::SEND)).unwrap();
            assert_eq!(ct.get(late, Rights::SEND).unwrap_err(), CapError::EndpointDead);
        }
    }

    /// backlog/90 D25: a cap minted on a dead record does not come back to life when the id is reused.
    /// An endpoint id is reused at `next_generation()`; the dead record's generation came from the same
    /// counter, so the next life is strictly above it whichever spawn took the counter's next value.
    #[test]
    fn a_cap_minted_on_a_dead_record_stays_dead_when_the_id_is_reused() {
        let id = ResourceId(3020);
        register_resource_at_gen(id, crate::capability::generation::next_generation());
        mark_dead_resource(id);
        let mut ct = CapTable::empty();
        let late = ct.insert(mint_cap(id, Rights::SEND)).unwrap();
        // The very next counter value goes to the id's next life, the case `bump()` alone collided with.
        register_resource_at_gen(id, crate::capability::generation::next_generation());
        assert_eq!(ct.get(late, Rights::SEND).unwrap_err(), CapError::EndpointDead,
            "a cap minted on the dead record validated against the id's next life");
        let fresh = ct.insert(mint_cap(id, Rights::SEND)).unwrap();
        assert!(ct.get(fresh, Rights::SEND).is_ok());
    }

    /// An id REUSED after its resource ended - registered again, alive, at a higher generation, as a
    /// respawn or any new spawn does with an endpoint id (LIFO). A stale cap to an ENDPOINT id is told
    /// its endpoint died; a stale cap to a DELEGATED id (a file) is told it was revoked. The endpoint case
    /// said CapRevoked until adversarial A5 caught it (2026-10-27).
    #[test]
    fn a_stale_cap_to_a_reused_id_is_dead_for_an_endpoint_and_revoked_for_a_file() {
        for (id, want) in [(3001u64, CapError::EndpointDead), (5010, CapError::CapRevoked)] {
            register_resource_at_gen(ResourceId(id), Generation(4));
            let mut ct = CapTable::empty();
            let slot = ct.insert(mint_cap(ResourceId(id), Rights::SEND)).unwrap();
            register_resource_at_gen(ResourceId(id), Generation(9)); // reused: alive, a later generation
            assert_eq!(ct.get(slot, Rights::SEND).unwrap_err(), want, "id {id}");
        }
    }

    #[test]
    fn get_of_an_unregistered_resource_is_not_held() {
        let mut ct = CapTable::empty();
        let slot = ct.insert(make_cap(5005, Rights::READ.0, 0)).unwrap();
        assert_eq!(ct.get(slot, Rights::READ).unwrap_err(), CapError::CapNotHeld);
    }

    #[test]
    fn a_resource_registered_at_a_generation_mints_at_it() {
        register_resource_at_gen(ResourceId(5006), Generation(9));
        assert_eq!(get_resource_generation(ResourceId(5006)).map(|g| g.0), Some(9));
        assert_eq!(mint_cap(ResourceId(5006), Rights::READ).generation.0, 9);
        assert!(get_resource_generation(ResourceId(5007)).is_none());
    }

    // The by-holdings gate of Kill, Reboot, PowerOff and the introspection reads.
    #[test]
    fn holds_resource_needs_the_resource_and_the_right() {
        let mut ct = CapTable::empty();
        ct.insert(make_cap(7, Rights::READ.0, 0)).unwrap();
        assert!(ct.holds_resource(ResourceId(7), Rights::READ));
        assert!(!ct.holds_resource(ResourceId(7), Rights::WRITE), "the right is part of the gate");
        assert!(!ct.holds_resource(ResourceId(8), Rights::READ), "so is the resource");
        assert!(!CapTable::empty().holds_resource(ResourceId(7), Rights::READ));
    }

    #[test]
    fn cap_read_rights_and_for_each_slot_see_what_is_held() {
        let mut ct = CapTable::empty();
        let a = ct.insert(make_cap(11, Rights::SEND.0, 0)).unwrap();
        ct.insert(make_cap(12, Rights::RECV.0, 0)).unwrap();
        assert_eq!(cap_read_rights(&ct, a), Some(Rights::SEND));
        assert_eq!(cap_read_rights(&ct, MAX_CAPS_PER_TASK - 1), None);
        let mut seen = Vec::new();
        ct.for_each_slot(|c| seen.push(c.resource_id.0));
        assert_eq!(seen, vec![11, 12]);
    }

    // Ids at and past DIRECT_CAP live in the overflow list: the boundary, two of them, and a bump.
    #[test]
    fn the_overflow_list_holds_ids_past_the_direct_array() {
        let mut t = Box::new(GlobalResourceTable::new());
        let (a, b) = (DIRECT_CAP as u64, DIRECT_CAP as u64 + 7);
        t.register(ResourceId(a));
        t.register(ResourceId(b));
        assert!(t.get_record(ResourceId(a)).is_some());
        assert!(t.get_record(ResourceId(b)).is_some());
        assert!(t.get_record(ResourceId(a + 1)).is_none());
        t.bump_generation(ResourceId(b), Liveness::Revoked);
        assert!(t.get_record(ResourceId(b)).unwrap().generation.0 >= 1, "an end moves the generation up");
        assert_eq!(t.get_record(ResourceId(b)).unwrap().liveness, Liveness::Revoked);
        assert_eq!(t.get_record(ResourceId(a)).unwrap().generation.0, 0, "a bump touches one record");
        // The FIRST overflow id is in the list, not past the end of the direct array.
        t.bump_generation(ResourceId(a), Liveness::Dead);
        assert!(t.get_record(ResourceId(a)).unwrap().generation.0 >= 1);
        t.bump_generation(ResourceId(DIRECT_CAP as u64 - 1), Liveness::Dead);
        assert!(t.get_record(ResourceId(DIRECT_CAP as u64 - 1)).is_none(), "an unregistered id is not created");
    }
}
