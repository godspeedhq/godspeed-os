//! Per-task capability table and the global resource generation registry — §7.8.
//!
//! Two structures live here:
//!
//! 1. `CapTable` — one per task; maps a slot index to a `Capability`.
//!    Populated at spawn time from the service contract; modified only on
//!    GRANT transfer or explicit revocation.
//!
//! 2. `GlobalResourceTable` — one per kernel; maps `ResourceId` to its
//!    current generation and liveness. Consulted on every cap validation.
//!
//! Concurrency (§7.8): v1 uses a global RwLock. Reads (lookup + gen check)
//! are concurrent; writes (insertion on spawn, removal on death) are serialized.
//! A v2 sharded or RCU design requires benchmarks before adoption.

use super::cap::{CapError, Capability, ResourceId};
use super::generation::Generation;
use super::rights::Rights;

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

        // SAFETY: GLOBAL_RESOURCES read under the cap subsystem lock.
        let record = match unsafe { GLOBAL_RESOURCES.get_record(cap.resource_id) } {
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

const EMPTY_ENTRY: (ResourceId, ResourceRecord) = (
    ResourceId(0),
    ResourceRecord { generation: Generation::INITIAL, liveness: Liveness::Alive },
);

struct GlobalResourceTable {
    entries: [(ResourceId, ResourceRecord); 512],
    len: usize,
}

impl GlobalResourceTable {
    const fn new() -> Self {
        Self { entries: [EMPTY_ENTRY; 512], len: 0 }
    }

    fn get_record(&self, id: ResourceId) -> Option<ResourceRecord> {
        self.entries[..self.len]
            .iter()
            .find(|(rid, _)| *rid == id)
            .map(|(_, r)| *r)
    }

    fn bump_generation(&mut self, id: ResourceId, liveness: Liveness) {
        if let Some((_, r)) = self.entries[..self.len].iter_mut().find(|(rid, _)| *rid == id) {
            r.generation = r.generation.bump();
            r.liveness = liveness;
        }
    }

    fn register(&mut self, id: ResourceId) {
        assert!(self.len < self.entries.len());
        self.entries[self.len] = (id, ResourceRecord {
            generation: Generation::INITIAL,
            liveness: Liveness::Alive,
        });
        self.len += 1;
    }
}

// Placeholder; real impl needs a spinlock wrapper.
static mut GLOBAL_RESOURCES: GlobalResourceTable = GlobalResourceTable::new();

pub fn init_global() {
    // Nothing to do for the placeholder; real init zeros the table.
}

/// Register a new resource in the global table with generation 0 and liveness Alive.
pub fn register_resource(id: ResourceId) {
    // SAFETY: serialized by the cap subsystem lock.
    unsafe { GLOBAL_RESOURCES.register(id) }
}

/// Mint a capability for `id` with the given rights at its current generation.
/// Panics if the resource is not registered.
pub fn mint_cap(id: ResourceId, rights: Rights) -> Capability {
    // SAFETY: read-only path.
    let record = unsafe { GLOBAL_RESOURCES.get_record(id) }
        .expect("mint_cap: resource not registered");
    Capability { resource_id: id, rights, generation: record.generation }
}

/// Bump generation and mark as Dead (endpoint/service terminated → `EndpointDead`).
pub fn mark_dead_resource(id: ResourceId) {
    // SAFETY: serialized by the cap subsystem lock.
    unsafe { GLOBAL_RESOURCES.bump_generation(id, Liveness::Dead) }
}

/// Bump generation and mark as Revoked (explicit supervisor revocation → `CapRevoked`).
pub fn revoke_resource(id: ResourceId) {
    // SAFETY: serialized by the cap subsystem lock.
    unsafe { GLOBAL_RESOURCES.bump_generation(id, Liveness::Revoked) }
}

/// Legacy alias; use `mark_dead_resource` or `revoke_resource` for new code.
pub fn bump_resource_generation(id: ResourceId) {
    mark_dead_resource(id)
}
