// SPDX-License-Identifier: GPL-2.0-only
//! Per-task capability table and the global resource generation registry - §7.8.
//!
//! Two structures live here:
//!
//! 1. `CapTable` - one per task; maps a slot index to a `Capability`.
//!    Populated at spawn time from the service contract; modified only on
//!    GRANT transfer or explicit revocation.
//!
//! 2. `GlobalResourceTable` - one per kernel; maps `ResourceId` to its
//!    current generation and liveness. Consulted on every cap validation.
//!
//! Concurrency (§7.8): v1 uses a global RwLock. Reads (lookup + gen check)
//! are concurrent; writes (insertion on spawn, removal on death) are serialized.
//! A v2 sharded or RCU design requires benchmarks before adoption.

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

        let record = match GLOBAL_RESOURCES.lock_irq().get_record(cap.resource_id) {
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
                Liveness::Alive   => CapError::CapRevoked, // gen mismatch with live resource
            });
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
    /// `LOG_WRITE`, `SPAWN`, `INTROSPECT`, …). Those are never revoked, so a held
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
        // SEC-11: no STABLE gate resource (ids 1-11: LOG_WRITE..ACQUIRE_ANY, NET_DEVICE, GPIO_DEVICE,
        // ever revoked or killed. `holds_resource` (the by-holdings gate for Kill/Reboot/ResourceMint/
        // Introspect/NetFrame*) validates those WITHOUT a generation check, which is sound only while they
        // stay un-revocable. Revocable ids are always >= 100 (endpoints) or in the delegated band. This
        // pins the invariant, so a future change that makes a gated resource revocable fails loudly
        // in test/debug rather than silently letting a revoked holder keep passing the gate.
        debug_assert!(id.0 >= 100,
            "SEC-11: bump_generation on stable gate resource {} would break holds_resource gen-safety", id.0);
        let i = id.0 as usize;
        if i < DIRECT_CAP {
            if self.present[i] {
                self.records[i].generation = self.records[i].generation.bump();
                self.records[i].liveness   = liveness;
            }
        } else if let Some((_, r)) = self.overflow[..self.overflow_len]
                .iter_mut().find(|(rid, _)| *rid == id) {
            r.generation = r.generation.bump();
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

        /// bump_generation is strictly monotonic per resource (§7.5 P2).
        #[test]
        fn bump_generation_is_strictly_monotonic(
            id    in 0u64..(DIRECT_CAP as u64),
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
        fn bump_to_dead_sets_liveness_dead(id in 0u64..(DIRECT_CAP as u64)) {
            let mut t = Box::new(GlobalResourceTable::new());
            t.register(ResourceId(id));
            t.bump_generation(ResourceId(id), Liveness::Dead);
            prop_assert_eq!(t.get_record(ResourceId(id)).unwrap().liveness, Liveness::Dead);
        }

        /// After bump_generation with Revoked liveness, get_record returns Revoked.
        #[test]
        fn bump_to_revoked_sets_liveness_revoked(id in 0u64..(DIRECT_CAP as u64)) {
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
}
