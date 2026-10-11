// SPDX-License-Identifier: GPL-2.0-only
//! IPC endpoint - §8.1, §8.3.
//!
//! An endpoint is owned by one service, pinned to one core. Its queue lives on
//! that core. Cross-core sends enqueue via the routing table + IPI path.

use crate::capability::cap::ResourceId;

/// Kernel-assigned unique identifier for an endpoint.
/// Used as the key in the routing table and as the `ResourceId` for the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u64);

impl From<EndpointId> for ResourceId {
    fn from(id: EndpointId) -> Self {
        ResourceId(id.0)
    }
}

// (An `Endpoint` struct sat here that nothing constructed: the live per-endpoint state - core,
// generation, liveness, queue, blocked receiver and sender - is `routing::RoutingEntry`, keyed by
// `EndpointId`. It was deleted 2026-10-10 (`backlog/80` K20).)
