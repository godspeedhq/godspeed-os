// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! IPC endpoint - §8.1, §8.3.
//!
//! An endpoint is owned by one service, pinned to one core. Its queue lives on
//! that core. Cross-core sends enqueue via the routing table + IPI path.

use crate::capability::cap::ResourceId;
use crate::ipc::queue::MessageQueue;
use crate::task::task::TaskId;

/// Kernel-assigned unique identifier for an endpoint.
/// Used as the key in the routing table and as the `ResourceId` for the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u64);

impl From<EndpointId> for ResourceId {
    fn from(id: EndpointId) -> Self {
        ResourceId(id.0)
    }
}

/// An IPC endpoint with its message queue and owner information.
pub struct Endpoint {
    pub id: EndpointId,
    /// The task that owns this endpoint (the receiver).
    pub owner: TaskId,
    /// Which core this endpoint is pinned to.
    pub core_id: u32,
    pub queue: MessageQueue,
    /// If a task is blocked on `recv`, its id is stored here so the kernel
    /// can wake it via IPI when a message arrives.
    pub blocked_receiver: Option<TaskId>,
}

impl Endpoint {
    pub fn new(id: EndpointId, owner: TaskId, core_id: u32) -> Self {
        Self {
            id,
            owner,
            core_id,
            queue: MessageQueue::new(),
            blocked_receiver: None,
        }
    }
}
