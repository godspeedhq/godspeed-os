//! Routing table: EndpointId → (CoreId, Generation, Liveness) — §8.3.
//!
//! Every send syscall consults this table to locate the target core and
//! check liveness before touching the endpoint's queue. The generation here
//! must match the cap generation or the send returns EndpointDead (§8.7).
//!
//! The table is global and protected by a spinlock. Reads are frequent (every
//! send); writes are rare (spawn/death). v2 may shard by core if contention
//! becomes measurable.

use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message};
use crate::task::task::TaskId;

pub fn init() {
    // Nothing needed for the placeholder; real init clears the table.
}

/// Look up the core that owns `endpoint`.
pub fn lookup_core(endpoint: EndpointId, cap_gen: Generation) -> Result<u32, IpcError> {
    todo!("find entry, check liveness, compare generations; return EndpointDead on mismatch")
}

/// Enqueue `msg` on the target endpoint. If the endpoint lives on a different
/// core, the caller must send an IPI after this returns Ok (§8.4).
pub fn enqueue(endpoint: EndpointId, msg: Message, cap_gen: Generation) -> Result<Option<TaskId>, IpcError> {
    todo!(
        "lock table, find endpoint queue, call queue.enqueue; \
         return blocked_receiver TaskId so caller can send IPI"
    )
}

/// Register a newly-created endpoint.
pub fn register(endpoint: EndpointId, core_id: u32, generation: Generation) {
    todo!("insert entry into the routing table")
}

/// Mark an endpoint dead and drain its queue (§8.6).
/// Called when the owning service is killed.
pub fn kill_endpoint(endpoint: EndpointId) {
    todo!("bump generation, set liveness=Dead, drain queue, wake any blocked sender with EndpointDead")
}
