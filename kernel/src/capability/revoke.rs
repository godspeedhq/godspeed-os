// SPDX-License-Identifier: GPL-2.0-only
//! Capability revocation - §7.5, §7.6.
//!
//! Revocation bumps the resource's generation in the global table. This
//! invalidates every outstanding cap to that resource on every core without
//! synchronous notification - the next use on any core returns `CapRevoked`
//! or `EndpointDead` via the generation mismatch path (§7.5).
//!
//! §7.4 reserves the `REVOKE` right to the supervisor. Today no cap carries it and nothing calls
//! `revoke` below: the live revocations are a delegated resource's owner revoking it
//! (`delegated::revoke_owned`, syscall 32) and the kill path marking a dead endpoint Dead.

use super::cap::ResourceId;
use super::table::revoke_resource;

/// Revoke all outstanding capabilities to `resource`.
///
/// Bumps the generation and marks liveness as `Revoked` so that the next
/// use of any stale cap returns `CapRevoked` (not `EndpointDead`).
/// Outstanding caps are not deleted from remote tasks' tables - lazy
/// invalidation is safe because the generation check is atomic (§7.5).
pub fn revoke(resource: ResourceId) {
    revoke_resource(resource);
}
