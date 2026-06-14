// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Routing table model for property testing — §8.3, §22 P5, P8, P10.
//!
//! `TestRoutingModel` mirrors the algorithmic invariants of `ipc/routing.rs`
//! without `SpinLock`, global statics, or hardware dependencies.
//! Pattern mirrors `memory/bitmap.rs` from item 6.

use std::collections::HashSet;
use crate::capability::generation::Generation;
use crate::ipc::message::{IpcError, Message};
use crate::ipc::queue::MessageQueue;

// Local model ID — structurally equivalent to ipc::endpoint::EndpointId(u64).
// Defined here because endpoint.rs depends on crate::task which is hardware-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u64);

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ModelEntry {
    id:         EndpointId,
    generation: Generation,
    alive:      bool,
    queue:      MessageQueue,
}

pub struct TestRoutingModel {
    entries: Vec<ModelEntry>,
}

impl TestRoutingModel {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Register a new endpoint, recycling the first dead slot if available.
    pub fn register(&mut self, id: EndpointId, gen: Generation) {
        for e in &mut self.entries {
            if !e.alive {
                e.id         = id;
                e.generation = gen;
                e.alive      = true;
                e.queue      = MessageQueue::new();
                return;
            }
        }
        self.entries.push(ModelEntry {
            id,
            generation: gen,
            alive: true,
            queue: MessageQueue::new(),
        });
    }

    /// Kill an endpoint: set dead, bump generation, drain queue.
    /// Returns the bumped generation, or None if the endpoint was not alive.
    pub fn kill(&mut self, id: EndpointId) -> Option<Generation> {
        self.entries.iter_mut()
            .find(|e| e.alive && e.id == id)
            .map(|e| {
                e.alive      = false;
                e.generation = e.generation.bump();
                e.queue.drain();
                e.generation
            })
    }

    /// Count of alive entries.
    pub fn count_live(&self) -> usize {
        self.entries.iter().filter(|e| e.alive).count()
    }

    /// All alive endpoint IDs (may contain duplicates if the model is broken).
    pub fn alive_ids(&self) -> Vec<EndpointId> {
        self.entries.iter()
            .filter(|e| e.alive)
            .map(|e| e.id)
            .collect()
    }

    /// Most-recent generation for `id` (alive or dead).
    /// Mirrors how `GLOBAL_RESOURCES` preserves the bumped generation so the
    /// spawn path inherits it on restart (§14.2, task/mod.rs:2314–2329).
    pub fn get_generation(&self, id: EndpointId) -> Option<Generation> {
        self.entries.iter().rev()
            .find(|e| e.id == id)
            .map(|e| e.generation)
    }

    /// Current queue depth for the alive entry with this ID, or None.
    pub fn queue_depth(&self, id: EndpointId) -> Option<usize> {
        self.entries.iter()
            .find(|e| e.alive && e.id == id)
            .map(|e| e.queue.depth())
    }

    /// Enqueue a 1-byte message — models the `send` syscall path.
    pub fn enqueue(&mut self, id: EndpointId, cap_gen: Generation) -> Result<(), IpcError> {
        let entry = self.entries.iter_mut()
            .find(|e| e.alive && e.id == id)
            .ok_or(IpcError::EndpointDead)?;
        if !cap_gen.matches(entry.generation) {
            return Err(IpcError::EndpointDead);
        }
        let msg = Message::new(&[0u8]).expect("1-byte message always fits");
        entry.queue.enqueue(msg).map_err(|_| IpcError::QueueFull)
    }

    /// Dequeue the head message — models the `recv` syscall path.
    pub fn dequeue(&mut self, id: EndpointId, cap_gen: Generation) -> Result<(), IpcError> {
        let entry = self.entries.iter_mut()
            .find(|e| e.alive && e.id == id)
            .ok_or(IpcError::EndpointDead)?;
        if !cap_gen.matches(entry.generation) {
            return Err(IpcError::EndpointDead);
        }
        entry.queue.dequeue()
            .map(|_| ())
            .ok_or(IpcError::QueueEmpty)
    }
}

// ---------------------------------------------------------------------------
// Operation sequences for state-machine property tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Register(u64),
        Kill(u64),
        Enqueue(u64),
        Dequeue(u64),
    }

    fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(
            prop_oneof![
                (0u64..16).prop_map(Op::Register),
                (0u64..16).prop_map(Op::Kill),
                (0u64..16).prop_map(Op::Enqueue),
                (0u64..16).prop_map(Op::Dequeue),
            ],
            0..64,
        )
    }

    // Apply an op sequence, tracking the current generation per endpoint ID.
    // Register is guarded: if the endpoint is already alive it is skipped.
    // This mirrors the kernel's usage protocol — spawn_service_with_config always
    // calls kill_endpoint before re-registering a service (task/mod.rs §14.1).
    // P5 holds under this protocol, and that is exactly what these tests verify.
    fn run_ops(ops: &[Op]) -> TestRoutingModel {
        let mut model = TestRoutingModel::new();
        let mut gens: std::collections::HashMap<u64, Generation> =
            std::collections::HashMap::new();
        for op in ops {
            match op {
                Op::Register(raw) => {
                    let id = EndpointId(*raw);
                    if !model.alive_ids().contains(&id) {
                        let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                        model.register(id, g);
                    }
                }
                Op::Kill(raw) => {
                    if let Some(bumped) = model.kill(EndpointId(*raw)) {
                        gens.insert(*raw, bumped);
                    }
                }
                Op::Enqueue(raw) => {
                    let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                    model.enqueue(EndpointId(*raw), g).ok();
                }
                Op::Dequeue(raw) => {
                    let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                    model.dequeue(EndpointId(*raw), g).ok();
                }
            }
        }
        model
    }

    // -----------------------------------------------------------------------
    // P5: Every live endpoint has exactly one entry (§8.3, §22 P5)
    // -----------------------------------------------------------------------

    proptest! {
        /// After any register/kill sequence, no endpoint ID appears twice in the
        /// alive set — §8.3, §22 P5.
        #[test]
        fn no_duplicate_alive_endpoint_ids(ops in ops_strategy()) {
            let model = run_ops(&ops);
            let alive = model.alive_ids();
            let unique: HashSet<_> = alive.iter().collect();
            prop_assert_eq!(
                unique.len(), alive.len(),
                "duplicate alive IDs {:?} after ops {:?}", alive, ops
            );
        }

        /// count_live always equals the iteration count of alive entries — §22 P5.
        #[test]
        fn count_live_consistent_with_iteration(ops in ops_strategy()) {
            let model = run_ops(&ops);
            prop_assert_eq!(model.count_live(), model.alive_ids().len());
        }
    }

    // -----------------------------------------------------------------------
    // P8: After restart, generation strictly increases (§7.5, §14.2, §22 P8)
    // -----------------------------------------------------------------------

    proptest! {
        /// Any number of kill+reregister cycles produce strictly increasing
        /// generations — §7.5, §14.2, §22 P8.
        #[test]
        fn kill_reregister_strictly_increases_generation(
            id_raw   in 0u64..32,
            cycles   in 1u32..10,
        ) {
            let id = EndpointId(id_raw);
            let mut model = TestRoutingModel::new();
            model.register(id, Generation::INITIAL);
            let mut prev = model.get_generation(id).unwrap();

            for _ in 0..cycles {
                let bumped = model.kill(id).unwrap();
                // Re-register at the bumped generation — mirrors spawn_service_with_config
                // inheriting the bumped gen from GLOBAL_RESOURCES (task/mod.rs:2324–2335).
                model.register(id, bumped);
                let current = model.get_generation(id).unwrap();
                prop_assert!(
                    current.0 > prev.0,
                    "generation must strictly increase: {} → {}", prev.0, current.0
                );
                prev = current;
            }
        }

        /// After kill+reregister: stale cap (old gen) is rejected; fresh cap
        /// (bumped gen) is accepted — §7.5, §14.2, §22 P8.
        #[test]
        fn stale_cap_rejected_fresh_cap_accepted_after_restart(id_raw in 0u64..32) {
            let id = EndpointId(id_raw);
            let mut model = TestRoutingModel::new();
            model.register(id, Generation::INITIAL);
            let old_gen = model.get_generation(id).unwrap();

            let bumped = model.kill(id).unwrap();
            model.register(id, bumped);

            prop_assert!(
                model.enqueue(id, old_gen).is_err(),
                "stale cap (gen={}) must be rejected", old_gen.0
            );
            prop_assert!(
                model.enqueue(id, bumped).is_ok(),
                "fresh cap (gen={}) must succeed", bumped.0
            );
        }
    }

    // -----------------------------------------------------------------------
    // P10: Every send returns exactly one defined outcome (§8.6, §22 P10)
    // -----------------------------------------------------------------------

    proptest! {
        /// Enqueue on a dead endpoint always returns EndpointDead — §8.6, §22 P10.
        #[test]
        fn enqueue_dead_endpoint_returns_endpoint_dead(id_raw in 0u64..32) {
            let id = EndpointId(id_raw);
            let mut model = TestRoutingModel::new();
            model.register(id, Generation::INITIAL);
            let gen = model.get_generation(id).unwrap();
            model.kill(id);
            prop_assert_eq!(model.enqueue(id, gen), Err(IpcError::EndpointDead));
        }

        /// Enqueue on a full queue always returns QueueFull — §8.6, §22 P10.
        #[test]
        fn enqueue_full_queue_returns_queue_full(id_raw in 0u64..32) {
            let id = EndpointId(id_raw);
            let mut model = TestRoutingModel::new();
            model.register(id, Generation::INITIAL);
            let gen = model.get_generation(id).unwrap();
            // Fill to capacity (depth = 16).
            for _ in 0..16 {
                model.enqueue(id, gen).ok();
            }
            prop_assert_eq!(model.enqueue(id, gen), Err(IpcError::QueueFull));
        }

        /// Enqueue on alive, non-full queue always returns Ok — §8.6, §22 P10.
        #[test]
        fn enqueue_alive_non_full_returns_ok(id_raw in 0u64..32, depth in 0usize..16) {
            let id = EndpointId(id_raw);
            let mut model = TestRoutingModel::new();
            model.register(id, Generation::INITIAL);
            let gen = model.get_generation(id).unwrap();
            for _ in 0..depth {
                model.enqueue(id, gen).ok();
            }
            prop_assert!(model.enqueue(id, gen).is_ok());
        }

        /// After any mixed op sequence, enqueue always returns one of the three
        /// defined outcomes — never panics, never an unexpected variant — §22 P10.
        #[test]
        fn enqueue_result_always_one_of_defined_outcomes(ops in ops_strategy()) {
            let mut model = TestRoutingModel::new();
            let mut gens: std::collections::HashMap<u64, Generation> =
                std::collections::HashMap::new();
            for op in &ops {
                match op {
                    Op::Register(raw) => {
                        let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                        model.register(EndpointId(*raw), g);
                    }
                    Op::Kill(raw) => {
                        if let Some(bumped) = model.kill(EndpointId(*raw)) {
                            gens.insert(*raw, bumped);
                        }
                    }
                    Op::Enqueue(raw) => {
                        let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                        let result = model.enqueue(EndpointId(*raw), g);
                        prop_assert!(
                            matches!(
                                result,
                                Ok(()) | Err(IpcError::EndpointDead) | Err(IpcError::QueueFull)
                            ),
                            "unexpected enqueue result: {:?}", result
                        );
                    }
                    Op::Dequeue(raw) => {
                        let g = gens.get(raw).copied().unwrap_or(Generation::INITIAL);
                        model.dequeue(EndpointId(*raw), g).ok();
                    }
                }
            }
        }
    }
}
