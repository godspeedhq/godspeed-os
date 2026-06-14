// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Capability revocation — §7.5, §7.6.
//!
//! Revocation bumps the resource's generation in the global table. This
//! invalidates every outstanding cap to that resource on every core without
//! synchronous notification — the next use on any core returns `CapRevoked`
//! or `EndpointDead` via the generation mismatch path (§7.5).
//!
//! Only the supervisor holds the `REVOKE` right (§7.4).

use super::cap::ResourceId;
use super::table::revoke_resource;

/// Revoke all outstanding capabilities to `resource`.
///
/// Bumps the generation and marks liveness as `Revoked` so that the next
/// use of any stale cap returns `CapRevoked` (not `EndpointDead`).
/// Outstanding caps are not deleted from remote tasks' tables — lazy
/// invalidation is safe because the generation check is atomic (§7.5).
pub fn revoke(resource: ResourceId) {
    revoke_resource(resource);
}
