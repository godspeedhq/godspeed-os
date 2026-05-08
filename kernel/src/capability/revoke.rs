//! Capability revocation — §7.5, §7.6.
//!
//! Revocation bumps the resource's generation in the global table. This
//! invalidates every outstanding cap to that resource on every core without
//! synchronous notification — the next use on any core returns `CapRevoked`
//! or `EndpointDead` via the generation mismatch path (§7.5).
//!
//! Only the supervisor holds the `REVOKE` right (§7.4).

use super::cap::ResourceId;
use super::table::bump_resource_generation;

/// Revoke all outstanding capabilities to `resource`.
///
/// The operation is a single generation bump. Outstanding caps are not
/// deleted from remote tasks' tables — they become stale and fail on
/// next use. This is intentional: lazy invalidation avoids cross-core
/// table writes and is safe because the generation check is atomic.
pub fn revoke(resource: ResourceId) {
    bump_resource_generation(resource);
}
