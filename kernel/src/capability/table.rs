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
    pub fn get(&self, slot: usize, required_right: Rights) -> Result<&Capability, CapError> {
        let cap = self.slots.get(slot)
            .and_then(|s| s.as_ref())
            .ok_or(CapError::CapNotHeld)?;

        // SAFETY: GLOBAL_RESOURCES read under the cap subsystem lock.
        let current_gen = unsafe { GLOBAL_RESOURCES.current_generation(cap.resource_id) }
            .ok_or(CapError::CapNotHeld)?;

        cap.validate(required_right, current_gen)?;
        Ok(cap)
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

    fn current_generation(&self, id: ResourceId) -> Option<Generation> {
        self.entries[..self.len]
            .iter()
            .find(|(rid, _)| *rid == id)
            .map(|(_, r)| r.generation)
    }

    fn bump_generation(&mut self, id: ResourceId) {
        if let Some((_, r)) = self.entries[..self.len].iter_mut().find(|(rid, _)| *rid == id) {
            r.generation = r.generation.bump();
            r.liveness = Liveness::Dead;
        }
    }

    fn register(&mut self, id: ResourceId) {
        assert!(self.len < self.entries.len());
        self.entries[self.len] = (id, ResourceRecord { generation: Generation::INITIAL, liveness: Liveness::Alive });
        self.len += 1;
    }
}

// Placeholder; real impl needs a spinlock wrapper.
static mut GLOBAL_RESOURCES: GlobalResourceTable = GlobalResourceTable::new();

pub fn init_global() {
    // Nothing to do for the placeholder; real init zeros the table.
}

pub fn register_resource(id: ResourceId) {
    // SAFETY: serialized by the cap subsystem lock.
    unsafe { GLOBAL_RESOURCES.register(id) }
}

pub fn bump_resource_generation(id: ResourceId) {
    // SAFETY: serialized by the cap subsystem lock.
    unsafe { GLOBAL_RESOURCES.bump_generation(id) }
}
